use crate::{Engine, config};
mod admin;
mod dictionary;
#[cfg(unix)]
use kanade_ipc::unix::{secure_directory, verify_peer};
#[cfg(windows)]
use kanade_ipc::windows::Pipe as NativeStream;
use kanade_ipc::{
    self as ipc,
    diagnostics::{Event, record},
    *,
};
use kanade_mozc::Mozc;
#[cfg(unix)]
use std::fs::{self, File};
#[cfg(unix)]
use std::os::unix::{
    fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
    net::{UnixListener, UnixStream as NativeStream},
};
use std::{
    io,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
    },
    time::{Duration, Instant},
};

enum Work {
    Date(Option<crate::dates::Calendar>),
    Dictionary(Box<dictionary::Command>),
    ClearMozcLearning(
        kanade_mozc::LearningKind,
        Instant,
        SyncSender<Result<(), failure::Code>>,
    ),
    Projects(Arc<kanade_indexer::Catalog>, SyncSender<()>),
    Configure(config::Config, SyncSender<()>),
    Open(Hello, SyncSender<Result<Welcome, failure::Code>>),
    Request(
        u64,
        Request,
        Instant,
        SyncSender<Result<Option<Response>, failure::Code>>,
    ),
    Close(u64),
}
/// Records why the process is about to exit. The error itself may name a
/// path, so only the stage reaches the log.
fn fatal<T, E: Into<anyhow::Error>>(event: Event, result: Result<T, E>) -> anyhow::Result<T> {
    result.map_err(|error| {
        record(event);
        error.into()
    })
}
/// Replaces a failed worker. One failed attempt does not end the engine: a
/// fresh process can answer its first calls slowly, and exiting leaves every
/// frontend without conversion until the service manager starts it again.
fn restart(mut attempt: impl FnMut() -> anyhow::Result<()>, pause: Duration) -> anyhow::Result<()> {
    const ATTEMPTS: u32 = 3;
    let mut round = 1;
    loop {
        match attempt() {
            Ok(()) => {
                record(Event::WorkerRestarted);
                return Ok(());
            }
            Err(error) if round == ATTEMPTS => {
                record(Event::FatalWorkerRestart);
                return Err(error);
            }
            Err(_) => {
                record(Event::WorkerRestartFailed);
                std::thread::sleep(pause * round);
                round += 1;
            }
        }
    }
}
/// History is written while input is idle. A composition on screen waits for
/// it: a slow or failed write must not cost the user that composition. One
/// left open for a long time does not postpone the write for ever.
fn sync_due(since_last: Duration, composing: bool) -> bool {
    since_last >= Duration::from_secs(30) && (!composing || since_last >= Duration::from_secs(300))
}
/// Bound admission without evicting an existing healthy connection. A paused
/// composition keeps its connection even when another peer wants to connect.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Default)]
struct Connections {
    next: u64,
    open: Vec<(u64, Instant, NativeStream)>,
}
#[cfg_attr(not(unix), allow(dead_code))]
impl Connections {
    const LIMIT: usize = 32;
    #[cfg(unix)]
    fn admit(&mut self, stream: &NativeStream) -> io::Result<u64> {
        if self.open.len() >= Self::LIMIT {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let stream = stream.try_clone()?;
        self.next += 1;
        self.open.push((self.next, Instant::now(), stream));
        Ok(self.next)
    }
    fn used(&mut self, token: u64) {
        if let Some(entry) = self.open.iter_mut().find(|entry| entry.0 == token) {
            entry.1 = Instant::now();
        }
    }
    fn closed(&mut self, token: u64) {
        self.open.retain(|entry| entry.0 != token);
    }
}
fn recover(engine: &mut Engine<Mozc>) -> anyhow::Result<()> {
    restart(|| engine.recover_worker(), Duration::from_millis(200))
}
pub fn serve(path: &Path, worker: &Path, data: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    let _lock = {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        fatal(Event::FatalSocketDirectory, secure_directory(parent))?;
        let lock = fatal(
            Event::FatalSocketDirectory,
            File::options()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .mode(0o600)
                .open(parent.join("engine.lock")),
        )?;
        fatal(
            Event::FatalAlreadyRunning,
            lock.try_lock()
                .map_err(|_| io::Error::from(io::ErrorKind::AddrInUse)),
        )?;
        fatal(
            Event::FatalSocketDirectory,
            match fs::symlink_metadata(path) {
                Ok(meta) if meta.file_type().is_socket() => fs::remove_file(path),
                Ok(_) => Err(io::Error::from(io::ErrorKind::PermissionDenied)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            },
        )?;
        lock
    };
    let settings = fatal(
        Event::FatalConfiguration,
        config::Config::load(&data.join("kanade/settings.toml")),
    )?;
    let project_changes = kanade_indexer::watch::catalog_changes(&data.join("kanade/projects"));
    let learning = Some(fatal(
        Event::FatalLearningStore,
        kanade_learn::service::LearningService::start(
            data.join("kanade/learning.json"),
            data.join("dict/ipadic.hsd"),
        ),
    )?);
    let backend = fatal(
        Event::FatalWorkerStart,
        Mozc::start(worker, &data.join("mozc")),
    )?;
    let projects =
        Arc::new(kanade_indexer::Catalog::load(&data.join("kanade/projects")).unwrap_or_default());
    // Model records are read and the worker is started on background threads.
    let llm = kanade_llm::Manager::open(
        data.join("kanade/llm"),
        kanade_llm::default_worker(),
        settings.llm,
    );
    let administration = Arc::new(admin::Administration::new(
        data.to_owned(),
        settings.clone(),
        learning.clone(),
        projects.clone(),
        llm.clone(),
    ));
    let mut engine = Engine::new(backend, learning);
    engine.set_reranker(Some(Box::new(llm.service())));
    engine.set_calendar(crate::dates::Calendar::local());
    engine.configure(settings);
    engine.set_projects(projects);
    #[cfg(unix)]
    let listener = fatal(Event::FatalListener, UnixListener::bind(path))?;
    #[cfg(unix)]
    fatal(
        Event::FatalListener,
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)),
    )?;
    #[cfg(windows)]
    let mut listener = fatal(Event::FatalListener, kanade_ipc::windows::Listener::bind())?;
    #[cfg(windows)]
    let _ = path;
    let (tx, rx) = mpsc::sync_channel::<Work>(64);
    {
        let administration = administration.clone();
        let tx = tx.clone();
        // Optional project startup performs no work on the input thread.
        let _ = std::thread::Builder::new()
            .name("kanade-project-start".into())
            .spawn(move || administration.start_projects(&tx));
    }
    {
        let tx = tx.clone();
        fatal(
            Event::FatalThread,
            std::thread::Builder::new()
                .name("kanade-calendar".into())
                .spawn(move || {
                    loop {
                        std::thread::sleep(Duration::from_secs(60));
                        if tx
                            .send(Work::Date(crate::dates::Calendar::local()))
                            .is_err()
                        {
                            break;
                        }
                    }
                }),
        )?;
    }
    match project_changes {
        Ok(changes) => {
            let administration = administration.clone();
            let tx = tx.clone();
            std::thread::Builder::new()
                .name("kanade-project-reload".into())
                .spawn(move || {
                    let mut retry: Option<Duration> = None;
                    while let Ok(changed) = changes.wait(retry.unwrap_or(Duration::from_secs(3600)))
                    {
                        if !changed && retry.is_none() {
                            continue;
                        }
                        match administration.reload_projects(&tx) {
                            Ok(false) => retry = None,
                            outcome => {
                                if outcome.is_err() {
                                    eprintln!("project reload failed; vocabulary disabled");
                                }
                                retry = Some(
                                    (retry.unwrap_or(Duration::from_millis(2500)) * 2)
                                        .min(Duration::from_secs(300)),
                                );
                            }
                        }
                    }
                })
                .inspect_err(|_| record(Event::FatalThread))?;
        }
        Err(_) => eprintln!("project notifications unavailable; explicit reload required"),
    }
    let active = Arc::new(AtomicUsize::new(0));
    let connections = Arc::new(Mutex::new(Connections::default()));
    std::thread::Builder::new()
        .name("kanade-listener".into())
        .spawn(move || {
            #[cfg(unix)]
            let incoming = listener.incoming().flatten();
            #[cfg(windows)]
            let incoming = std::iter::from_fn(move || listener.accept().ok());
            for stream in incoming {
                if active
                    .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                        (n < 32).then_some(n + 1)
                    })
                    .is_err()
                {
                    continue;
                }
                let active = active.clone();
                let tx = tx.clone();
                let administration = administration.clone();
                let connections = connections.clone();
                let result = std::thread::Builder::new()
                    .name("kanade-connection".into())
                    .spawn(move || {
                        let _ = connection(stream, &tx, &administration, &connections);
                        active.fetch_sub(1, Ordering::AcqRel);
                    });
                if result.is_err() {
                    break;
                }
            }
        })
        .inspect_err(|_| record(Event::FatalThread))?;
    record(Event::EngineStarted);
    let mut last_sync = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Work::Date(date)) => engine.set_calendar(date),
            Ok(Work::Dictionary(command)) => {
                if command.execute(&mut engine) {
                    recover(&mut engine)?;
                }
            }
            Ok(Work::ClearMozcLearning(kind, deadline, reply)) => {
                if Instant::now() > deadline {
                    let _ = reply.send(Err(failure::Code::Unavailable));
                    continue;
                }
                let result = engine.clear_mozc_learning(kind);
                let failed = result == Err(failure::Code::Unavailable) && !engine.worker_healthy();
                let _ = reply.send(result);
                if failed {
                    recover(&mut engine)?;
                }
            }
            Ok(Work::Projects(projects, ack)) => {
                engine.set_projects(projects);
                let _ = ack.send(());
            }
            Ok(Work::Configure(settings, ack)) => {
                engine.configure(settings);
                let _ = ack.send(());
            }
            Ok(Work::Open(hello, reply)) => {
                let result = engine.open(&hello);
                let failed = result == Err(failure::Code::Unavailable) && !engine.worker_healthy();
                let _ = reply.send(result);
                if failed {
                    recover(&mut engine)?;
                }
            }
            Ok(Work::Request(id, request, deadline, reply)) => {
                if Instant::now() > deadline {
                    // Refused before any of it ran; the session is unchanged.
                    record(Event::RequestExpiredInQueue);
                    let _ = reply.send(Err(failure::Code::Unavailable));
                    continue;
                }
                let result = engine.handle(id, request);
                // Only a worker that stopped answering is replaced. Replacing
                // it ends every composition, so a failure confined to one
                // session must not do that to the others.
                let failed = result == Err(failure::Code::Unavailable) && !engine.worker_healthy();
                let _ = reply.send(result);
                if failed {
                    recover(&mut engine)?;
                }
            }
            Ok(Work::Close(id)) => engine.close(id),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if sync_due(last_sync.elapsed(), engine.composing()) {
                    if engine.sync().is_err() {
                        record(Event::SyncFailed);
                        recover(&mut engine)?;
                    }
                    last_sync = Instant::now();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = engine.sync();
                fatal(Event::FatalLearningFlush, engine.flush_learning())?;
                return Ok(());
            }
        }
    }
}
fn connection(
    mut stream: NativeStream,
    tx: &SyncSender<Work>,
    admin: &admin::Administration,
    connections: &Mutex<Connections>,
) -> io::Result<()> {
    #[cfg(unix)]
    verify_peer(&stream)?;
    #[cfg(unix)]
    stream.set_read_timeout(Some(ipc::FRAME_TIMEOUT))?;
    #[cfg(unix)]
    stream.set_write_timeout(Some(ipc::KEY_TIMEOUT))?;
    #[cfg(windows)]
    stream.begin(ipc::FRAME_TIMEOUT);
    let hello: Request = ipc::receive(&mut stream)?;
    let Some(request::Body::Hello(hello)) = hello.body else {
        return Err(io::ErrorKind::InvalidData.into());
    };
    let (reply, response) = mpsc::sync_channel(1);
    tx.try_send(Work::Open(hello, reply))
        .map_err(|_| io::ErrorKind::WouldBlock)?;
    let welcome = response
        .recv_timeout(ipc::REQUEST_TIMEOUT)
        .map_err(|_| io::ErrorKind::TimedOut)?
        .map_err(|_| io::ErrorKind::InvalidData)?;
    let id = welcome.connection;
    #[cfg(unix)]
    let token = match connections
        .lock()
        .map_err(|_| io::Error::from(io::ErrorKind::Other))
        .and_then(|mut c| c.admit(&stream))
    {
        Ok(token) => Some(token),
        Err(error) => {
            let _ = tx.send(Work::Close(id));
            return Err(error);
        }
    };
    #[cfg(windows)]
    let token: Option<u64> = None;
    let result = (|| {
        #[cfg(windows)]
        stream.begin(ipc::KEY_TIMEOUT);
        ipc::send(
            &mut stream,
            &Response {
                body: Some(response::Body::Welcome(welcome)),
            },
        )?;
        loop {
            // No limit while the peer is quiet. Once a frame begins, all of it
            // must arrive in time, so a stalled sender cannot hold this
            // thread. Request deadlines are separate and start below.
            #[cfg(unix)]
            let frame = ipc::unix::read_idle_frame(&stream, ipc::FRAME_TIMEOUT)?;
            #[cfg(windows)]
            let frame = stream.read_idle_frame(ipc::FRAME_TIMEOUT)?;
            let Some(frame) = frame else {
                break;
            };
            if let (Some(token), Ok(mut connections)) = (token, connections.lock()) {
                connections.used(token);
            }
            #[cfg(windows)]
            stream.begin(ipc::KEY_TIMEOUT);
            let request: Request = ipc::decode(&frame)?;
            if matches!(request.body, Some(request::Body::Close(_))) {
                break;
            }
            if let Some(request::Body::Manage(manage)) = request.body.as_ref() {
                #[cfg(windows)]
                stream.verify_management_peer(
                    &std::env::current_exe()?.with_file_name("kanade-ui.exe"),
                )?;
                let json = admin.handle(manage, tx)?;
                #[cfg(windows)]
                stream.begin(Duration::from_secs(5));
                ipc::send(
                    &mut stream,
                    &Response {
                        body: Some(response::Body::Management(Management { json })),
                    },
                )?;
                continue;
            }
            let one_way = matches!(request.body, Some(request::Body::Ack(_)));
            let (reply, response) = mpsc::sync_channel(1);
            tx.try_send(Work::Request(
                id,
                request,
                Instant::now() + ipc::QUEUE_TIMEOUT,
                reply,
            ))
            .map_err(|_| io::ErrorKind::WouldBlock)?;
            // Longer than the queue limit plus the request's worker budget, so
            // an answer or a refusal normally arrives first.
            let answer = response
                .recv_timeout(ipc::REQUEST_TIMEOUT)
                .inspect_err(|_| record(Event::RequestAbandonedByConnection))
                .map_err(|_| io::ErrorKind::TimedOut)?;
            #[cfg(windows)]
            stream.begin(ipc::KEY_TIMEOUT);
            match answer {
                Ok(Some(reply)) => ipc::send(&mut stream, &reply)?,
                Ok(None) if one_way => {}
                Ok(None) => return Err(io::ErrorKind::InvalidData.into()),
                Err(code) => {
                    if !one_way {
                        ipc::send(
                            &mut stream,
                            &Response {
                                body: Some(response::Body::Failure(Failure { code: code as i32 })),
                            },
                        )?;
                    }
                    return Err(io::ErrorKind::InvalidData.into());
                }
            }
        }
        Ok(())
    })();
    if let (Some(token), Ok(mut connections)) = (token, connections.lock()) {
        connections.closed(token);
    }
    let _ = tx.send(Work::Close(id));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_sync_waits_for_an_open_composition_but_not_for_ever() {
        let seconds = Duration::from_secs;
        assert!(!sync_due(seconds(29), false));
        assert!(sync_due(seconds(30), false));
        assert!(!sync_due(seconds(30), true));
        assert!(!sync_due(seconds(299), true));
        assert!(sync_due(seconds(300), true));
    }
    #[cfg(unix)]
    #[test]
    fn a_full_table_preserves_existing_connections_and_refuses_new_admission() {
        use std::io::Read;
        let mut connections = Connections::default();
        let mut peers = Vec::new();
        for _ in 0..Connections::LIMIT {
            let (server, client) = NativeStream::pair().unwrap();
            let token = connections.admit(&server).unwrap();
            peers.push((token, server, client));
        }
        // Nothing is closed for being idle while there is room.
        for (_, _, client) in &peers {
            client.set_nonblocking(true).unwrap();
            let mut byte = [0];
            let idle = (&*client).read(&mut byte).unwrap_err();
            assert_eq!(idle.kind(), io::ErrorKind::WouldBlock);
        }
        // Capacity pressure cannot evict a paused or active composition.
        connections.used(peers[0].0);
        let (server, _client) = NativeStream::pair().unwrap();
        assert_eq!(
            connections.admit(&server).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(connections.open.len(), Connections::LIMIT);
        let mut byte = [0];
        for (_, _, client) in &peers {
            let quiet = (&*client).read(&mut byte).unwrap_err();
            assert_eq!(quiet.kind(), io::ErrorKind::WouldBlock);
        }
        connections.closed(peers[0].0);
        assert_eq!(connections.open.len(), Connections::LIMIT - 1);
        connections.admit(&server).unwrap();
        assert_eq!(connections.open.len(), Connections::LIMIT);
    }
    #[test]
    fn one_failed_worker_replacement_does_not_end_the_engine() {
        let mut calls = 0;
        restart(
            || {
                calls += 1;
                anyhow::ensure!(calls == 3, "the fresh worker answered late");
                Ok(())
            },
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(calls, 3);
    }
    #[test]
    fn a_worker_that_never_returns_still_ends_the_engine() {
        let mut calls = 0;
        let result = restart(
            || {
                calls += 1;
                anyhow::bail!("the worker cannot start")
            },
            Duration::ZERO,
        );
        assert!(result.is_err());
        assert_eq!(calls, 3);
    }
}
