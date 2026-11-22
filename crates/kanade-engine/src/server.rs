use crate::{Engine, config};
mod admin;
mod dictionary;
#[cfg(unix)]
use kanade_ipc::unix::{secure_directory, verify_peer};
#[cfg(windows)]
use kanade_ipc::windows::Pipe as NativeStream;
use kanade_ipc::{
    self as ipc,
    diagnostics::{Event, Thread, record},
    *,
};
use kanade_mozc::Mozc;
#[cfg(unix)]
use std::fs;
use std::fs::File;
#[cfg(unix)]
use std::os::unix::{
    fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
    net::{UnixListener, UnixStream as NativeStream},
};
use std::{
    collections::VecDeque,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
    },
    time::{Duration, Instant},
};

/// Another engine already serves this endpoint or uses this data directory.
/// The process exits with its own code, so a supervisor can tell this from a
/// failure.
#[derive(Debug)]
pub struct AlreadyRunning;
impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("another engine is already running")
    }
}
impl std::error::Error for AlreadyRunning {}

/// Deliberate failures for the tests of the process's own survival. Compiled
/// only with the `fault-injection` feature, which no shipped build enables.
#[cfg(feature = "fault-injection")]
pub mod fault {
    /// A key with this text panics on the thread that handles every request.
    pub const PANIC_KEY: &str = "\u{1}panic";
    /// Stands for text a real panic could quote. It must never be recorded.
    pub const PANIC_MESSAGE: &str = "injected fault quoting ひみつのよみ";
    pub(super) fn check(request: &kanade_ipc::Request) {
        if let Some(kanade_ipc::request::Body::Key(key)) = &request.body
            && key.key.as_ref().is_some_and(|key| key.text == PANIC_KEY)
        {
            panic!("{PANIC_MESSAGE}");
        }
    }
}

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
    /// The listener gave up; nobody can connect to this process any more.
    #[cfg_attr(not(windows), allow(dead_code))]
    ListenerLost,
}
/// Records why the process is about to exit. The error itself may name a
/// path, so only the stage reaches the log.
fn fatal<T, E: Into<anyhow::Error>>(event: Event, result: Result<T, E>) -> anyhow::Result<T> {
    result.map_err(|error| {
        record(event);
        error.into()
    })
}
/// The settings file of a data directory, with the separators of the system
/// since the settings application shows it.
fn settings_file(data: &Path) -> PathBuf {
    data.join("kanade").join("settings.toml")
}
/// The learning store of a data directory.
fn learning_file(data: &Path) -> PathBuf {
    data.join("kanade").join("learning.json")
}
/// Reads the settings file and starts the learning store. Neither one that
/// exists but cannot be read stops the engine, or is written while it runs:
/// it may come from a newer version or be under repair. Settings that cannot
/// be read are `None`, and the engine keeps every optional feature off; a
/// store that cannot be read leaves Kanade's correction learning off.
fn open_stores(
    data: &Path,
) -> anyhow::Result<(
    Option<config::Config>,
    Option<kanade_learn::service::LearningService>,
)> {
    let settings = config::Config::load(&settings_file(data))
        .inspect_err(|_| record(Event::SettingsUnreadable))
        .ok();
    let path = learning_file(data);
    let learning = match kanade_learn::Store::load(&path) {
        // A store that was read is not reported as unreadable when only its
        // thread cannot start.
        Ok(store) => Some(fatal(
            Event::FatalThread,
            kanade_learn::service::LearningService::with_store(
                store,
                path,
                data.join("dict/ipadic.hsd"),
            ),
        )?),
        Err(_) => {
            record(Event::LearningStoreUnreadable);
            None
        }
    };
    Ok((settings, learning))
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
/// One admitted connection. The slot is returned when its thread ends,
/// however it ends: a panic there must not use up the limit for good.
struct Slot(Arc<AtomicUsize>);
impl Slot {
    const LIMIT: usize = 32;
    fn take(active: &Arc<AtomicUsize>) -> Option<Self> {
        active
            .try_update(Ordering::AcqRel, Ordering::Acquire, |taken| {
                (taken < Self::LIMIT).then_some(taken + 1)
            })
            .ok()
            .map(|_| Self(active.clone()))
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
/// The engine session of one connection. It is closed when the connection's
/// thread ends, however it ends, so the session cannot outlive its frontend.
struct Leaving<'a>(&'a SyncSender<Work>, u64);
impl Drop for Leaving<'_> {
    fn drop(&mut self) {
        let _ = self.0.send(Work::Close(self.1));
    }
}
/// A connection's entry in the table, removed when its thread ends.
struct Admitted<'a>(&'a Mutex<Connections>, Option<u64>);
impl Drop for Admitted<'_> {
    fn drop(&mut self) {
        if let (Some(token), Ok(mut connections)) = (self.1, self.0.lock()) {
            connections.closed(token);
        }
    }
}
/// Hands one accepted peer to its own thread, or turns it away.
fn admit(
    stream: NativeStream,
    active: &Arc<AtomicUsize>,
    tx: &SyncSender<Work>,
    administration: &Arc<admin::Administration>,
    connections: &Arc<Mutex<Connections>>,
) {
    let Some(slot) = Slot::take(active) else {
        // Dropping the stream closes it; the frontend tries again later.
        record(Event::ConnectionRefusedAtCapacity);
        return;
    };
    let tx = tx.clone();
    let administration = administration.clone();
    let connections = connections.clone();
    let spawned = std::thread::Builder::new()
        .name(Thread::CONNECTION.into())
        .spawn(move || {
            let _slot = slot;
            let _ = connection(stream, &tx, &administration, &connections);
        });
    if spawned.is_err() {
        // This peer is lost, with its slot returned. Later ones may fare better.
        record(Event::ConnectionThreadFailed);
    }
}
/// Panics close together mean that the state of this process cannot be
/// trusted any more, and that a new process is the better recovery.
#[derive(Default)]
struct Storm(VecDeque<Instant>);
impl Storm {
    const WINDOW: Duration = Duration::from_secs(60);
    const TOLERATED: usize = 4;
    /// Notes one panic. True once more than the tolerated number fell into
    /// one window.
    fn note(&mut self, now: Instant) -> bool {
        self.0.push_back(now);
        while self
            .0
            .front()
            .is_some_and(|first| now.duration_since(*first) > Self::WINDOW)
        {
            self.0.pop_front();
        }
        self.0.len() > Self::TOLERATED
    }
}
enum Flow {
    Continue,
    Stop,
}
/// Runs one turn of the main loop. A panic inside it ends that turn, not the
/// engine: `None` tells the caller to discard what the turn may have left
/// half done. The panic hook has already recorded where it started. Its
/// message can quote the text that was being handled, so it is dropped unread.
fn contain(
    turn: impl FnOnce() -> anyhow::Result<Flow>,
    storm: &mut Storm,
) -> anyhow::Result<Option<Flow>> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(turn)) {
        Ok(flow) => flow.map(Some),
        Err(message) => {
            drop(message);
            if storm.note(Instant::now()) {
                record(Event::FatalRepeatedPanic);
                anyhow::bail!("panics kept recurring");
            }
            Ok(None)
        }
    }
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
            lock.try_lock().map_err(|_| AlreadyRunning),
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
    // Two engines must not share a learning store and a conversion profile.
    // The pipe name cannot tell: an isolated instance has its own.
    #[cfg(windows)]
    let _lock = {
        let lock = fatal(
            Event::FatalConfiguration,
            std::fs::create_dir_all(data).and_then(|()| {
                File::options()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(data.join("engine.lock"))
            }),
        )?;
        fatal(
            Event::FatalAlreadyRunning,
            lock.try_lock().map_err(|_| AlreadyRunning),
        )?;
        lock
    };
    let (settings, learning) = open_stores(data)?;
    let project_changes = kanade_indexer::watch::catalog_changes(&data.join("kanade/projects"));
    let backend = fatal(
        Event::FatalWorkerStart,
        Mozc::start(worker, &data.join("mozc")),
    )?;
    let projects =
        Arc::new(kanade_indexer::Catalog::load(&data.join("kanade/projects")).unwrap_or_default());
    let in_force = settings.clone().unwrap_or_else(config::Config::unreadable);
    // Model records are read and the worker is started on background threads.
    let llm = kanade_llm::Manager::open(
        data.join("kanade/llm"),
        kanade_llm::default_worker(),
        in_force.llm,
    );
    let administration = Arc::new(admin::Administration::new(
        data.to_owned(),
        settings,
        learning.clone(),
        projects.clone(),
        llm.clone(),
    ));
    let mut engine = Engine::new(backend, learning);
    engine.set_reranker(Some(Box::new(llm.service())));
    engine.set_calendar(crate::dates::Calendar::local());
    engine.configure(in_force);
    engine.set_projects(projects);
    #[cfg(unix)]
    let listener = fatal(Event::FatalListener, UnixListener::bind(path))?;
    #[cfg(unix)]
    fatal(
        Event::FatalListener,
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)),
    )?;
    // The pipe admits packaged applications, and a frontend connects only
    // after it has checked which program serves the pipe. Without this a
    // text service inside such an application could open the pipe and
    // still never convert. Failing here costs those hosts only.
    #[cfg(windows)]
    if kanade_ipc::windows::identify_to_packages().is_err() {
        record(Event::SandboxedHostsUnreachable);
    }
    #[cfg(windows)]
    let mut listener = match path
        .to_str()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))
        .and_then(kanade_ipc::windows::Listener::bind_to)
    {
        Ok(listener) => listener,
        // ERROR_ACCESS_DENIED: the name already has its first instance, which
        // belongs to an engine with another data directory.
        Err(error) if error.raw_os_error() == Some(5) => {
            return fatal(Event::FatalAlreadyRunning, Err(AlreadyRunning));
        }
        Err(error) => return fatal(Event::FatalListener, Err(error)),
    };
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
        .name(Thread::LISTENER.into())
        .spawn(move || {
            #[cfg(unix)]
            for stream in listener.incoming().flatten() {
                admit(stream, &active, &tx, &administration, &connections);
            }
            // One peer that cannot be accepted says nothing about the next.
            // Only a listener that stays unable to offer a pipe instance ends
            // the engine, so that a new process can take the endpoint: a
            // process nobody can connect to is worse than none.
            #[cfg(windows)]
            {
                use kanade_ipc::windows::Refusal;
                const ATTEMPTS: u32 = 10;
                let mut failures = 0;
                loop {
                    match listener.accept() {
                        Ok(stream) => {
                            failures = 0;
                            admit(stream, &active, &tx, &administration, &connections);
                        }
                        Err(Refusal::Peer(_)) => {
                            failures = 0;
                            record(Event::ConnectionRejected);
                        }
                        Err(Refusal::Listener(_)) => {
                            record(Event::ListenerFailed);
                            failures += 1;
                            if failures == ATTEMPTS {
                                let _ = tx.send(Work::ListenerLost);
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(100) * failures);
                        }
                    }
                }
            }
        })
        .inspect_err(|_| record(Event::FatalThread))?;
    record(Event::EngineStarted);
    let mut last_sync = Instant::now();
    let mut storm = Storm::default();
    loop {
        let work = rx.recv_timeout(Duration::from_secs(5));
        match contain(|| turn(&mut engine, work, &mut last_sync), &mut storm)? {
            Some(Flow::Continue) => {}
            Some(Flow::Stop) => return Ok(()),
            None => {
                // Any session may hold state from the interrupted turn.
                // Dropping them all and replacing the worker is the recovery
                // every frontend already handles: it holds its composition
                // and restores it on a new connection.
                recover(&mut engine)?;
                record(Event::RecoveredFromPanic);
            }
        }
    }
}
/// One item of work on the thread that owns the engine.
fn turn(
    engine: &mut Engine<Mozc>,
    work: Result<Work, mpsc::RecvTimeoutError>,
    last_sync: &mut Instant,
) -> anyhow::Result<Flow> {
    match work {
        Ok(Work::Date(date)) => engine.set_calendar(date),
        Ok(Work::Dictionary(command)) => {
            if command.execute(engine) {
                recover(engine)?;
            }
        }
        Ok(Work::ClearMozcLearning(kind, deadline, reply)) => {
            if Instant::now() > deadline {
                let _ = reply.send(Err(failure::Code::Unavailable));
                return Ok(Flow::Continue);
            }
            let result = engine.clear_mozc_learning(kind);
            let failed = result == Err(failure::Code::Unavailable) && !engine.worker_healthy();
            let _ = reply.send(result);
            if failed {
                recover(engine)?;
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
                recover(engine)?;
            }
        }
        Ok(Work::Request(id, request, deadline, reply)) => {
            if Instant::now() > deadline {
                // Refused before any of it ran; the session is unchanged.
                record(Event::RequestExpiredInQueue);
                let _ = reply.send(Err(failure::Code::Unavailable));
                return Ok(Flow::Continue);
            }
            #[cfg(feature = "fault-injection")]
            fault::check(&request);
            let result = engine.handle(id, request);
            // Only a worker that stopped answering is replaced. Replacing
            // it ends every composition, so a failure confined to one
            // session must not do that to the others.
            let failed = result == Err(failure::Code::Unavailable) && !engine.worker_healthy();
            let _ = reply.send(result);
            if failed {
                recover(engine)?;
            }
        }
        Ok(Work::Close(id)) => engine.close(id),
        Ok(Work::ListenerLost) => {
            let _ = engine.sync();
            let _ = engine.flush_learning();
            return fatal(
                Event::FatalListenerLost,
                Err(io::Error::from(io::ErrorKind::BrokenPipe)),
            );
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            if sync_due(last_sync.elapsed(), engine.composing()) {
                if engine.sync().is_err() {
                    record(Event::SyncFailed);
                    recover(engine)?;
                }
                *last_sync = Instant::now();
            }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = engine.sync();
            fatal(Event::FatalLearningFlush, engine.flush_learning())?;
            return Ok(Flow::Stop);
        }
    }
    Ok(Flow::Continue)
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
    let _leaving = Leaving(tx, id);
    #[cfg(unix)]
    let token = Some(
        connections
            .lock()
            .map_err(|_| io::Error::from(io::ErrorKind::Other))
            .and_then(|mut c| c.admit(&stream))?,
    );
    #[cfg(windows)]
    let token: Option<u64> = None;
    let _admitted = Admitted(connections, token);
    {
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
                let management = admin.handle(manage, tx)?;
                #[cfg(windows)]
                stream.begin(Duration::from_secs(5));
                ipc::send(
                    &mut stream,
                    &Response {
                        body: Some(response::Body::Management(management)),
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
            // an answer or a refusal normally arrives first. A reply that was
            // dropped unsent means the turn handling it was interrupted;
            // that is recorded where it happened, and the frontend learns of
            // it at once from its closed connection.
            let answer = response
                .recv_timeout(ipc::REQUEST_TIMEOUT)
                .map_err(|error| {
                    if error == mpsc::RecvTimeoutError::Timeout {
                        record(Event::RequestAbandonedByConnection);
                        io::ErrorKind::TimedOut
                    } else {
                        io::ErrorKind::ConnectionAborted
                    }
                })?;
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
    }
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
    #[test]
    fn files_that_cannot_be_read_neither_stop_the_engine_nor_are_written() {
        let data = tempfile::tempdir().unwrap();
        let (settings, learning) = open_stores(data.path()).unwrap();
        assert!(settings.unwrap().learning, "a first start has the defaults");
        assert!(learning.is_some());
        drop(learning);
        let settings_file = data.path().join("kanade/settings.toml");
        let learning_file = data.path().join("kanade/learning.json");
        std::fs::create_dir_all(settings_file.parent().unwrap()).unwrap();
        for (settings_text, learning_text) in [
            // A newer version wrote a setting this one does not know.
            (
                "learning = true\nfrom_a_newer_version = 1\n",
                "{\"records\":{",
            ),
            // Both files end in the middle of a value.
            ("learning = true\nfont_size = ", "{\"records\":[{\"id\":"),
        ] {
            std::fs::write(&settings_file, settings_text).unwrap();
            std::fs::write(&learning_file, learning_text).unwrap();
            let (settings, learning) = open_stores(data.path()).unwrap();
            assert!(settings.is_none());
            assert!(learning.is_none());
            assert_eq!(
                std::fs::read_to_string(&settings_file).unwrap(),
                settings_text
            );
            assert_eq!(
                std::fs::read_to_string(&learning_file).unwrap(),
                learning_text
            );
        }
    }
    #[test]
    fn settings_that_cannot_be_read_are_never_saved_and_read_again_on_request() {
        let data = tempfile::tempdir().unwrap();
        let file = data.path().join("kanade/settings.toml");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let broken = "learning = true\nfrom_a_newer_version = 1\n";
        std::fs::write(&file, broken).unwrap();
        let llm = kanade_llm::Manager::open(
            data.path().join("kanade/llm"),
            data.path().join("no-worker"),
            false,
        );
        let admin =
            admin::Administration::new(data.path().to_owned(), None, None, Arc::default(), llm);
        // Applies configuration as the engine thread does.
        let (tx, rx) = mpsc::sync_channel::<Work>(4);
        let engine = std::thread::spawn(move || {
            let mut configured = vec![];
            while let Ok(work) = rx.recv() {
                if let Work::Configure(settings, ack) = work {
                    configured.push(settings);
                    let _ = ack.send(());
                }
            }
            configured
        });
        let ask = |action: manage::Action, json: String| {
            admin.handle(
                &Manage {
                    action: action as i32,
                    json,
                    ..Default::default()
                },
                &tx,
            )
        };
        let shown = |reply: io::Result<Management>| {
            let reply = reply.unwrap();
            let settings = serde_json::from_str::<config::Config>(&reply.json).unwrap();
            (settings, reply.settings, reply.learning)
        };
        let unreadable = StoreState::Unreadable as i32;
        let reply = ask(manage::Action::GetSettings, String::new()).unwrap();
        // The reply names the files this engine opened, for a settings
        // application that may run with another data directory.
        let named = |name| {
            let file = data.path().join("kanade").join(name);
            file.to_string_lossy().into_owned()
        };
        assert_eq!(reply.settings_file, named("settings.toml"));
        assert_eq!(reply.learning_file, named("learning.json"));
        let (fallback, settings, learning) = shown(Ok(reply));
        assert_eq!((settings, learning), (unreadable, unreadable));
        assert!(!fallback.learning && !fallback.emoji_dictionary);
        let edited = serde_json::to_string(&config::Config {
            font_size: 20.0,
            ..fallback
        })
        .unwrap();
        assert!(ask(manage::Action::SetSettings, edited).is_err());
        let exclusions = serde_json::json!({"previous": [], "apps": ["com.example.Editor"]});
        assert!(
            ask(
                manage::Action::SetLearningExclusions,
                exclusions.to_string()
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), broken);
        // Once repaired, the next request takes the file up, and settings
        // changed from what it then shows can be saved.
        std::fs::write(&file, "font_size = 24.0\n").unwrap();
        let reply = ask(manage::Action::GetSettings, String::new()).unwrap();
        assert_eq!(reply.settings_file, "");
        assert_eq!(reply.learning_file, named("learning.json"));
        let (repaired, settings, _) = shown(Ok(reply));
        assert_eq!(settings, StoreState::Read as i32);
        assert_eq!(repaired.font_size, 24.0);
        assert!(repaired.learning && repaired.emoji_dictionary);
        let edited = serde_json::to_string(&config::Config {
            font_size: 20.0,
            ..repaired
        })
        .unwrap();
        ask(manage::Action::SetSettings, edited).unwrap();
        let saved = config::Config::load(&file).unwrap();
        assert_eq!(saved.font_size, 20.0);
        assert!(saved.learning && saved.emoji_dictionary);
        drop(tx);
        let configured = engine.join().unwrap();
        assert_eq!(
            configured.iter().map(|c| c.font_size).collect::<Vec<_>>(),
            [24.0, 20.0]
        );
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
    #[test]
    fn a_panic_ends_its_turn_and_the_waiting_connection_learns_at_once() {
        let mut storm = Storm::default();
        let (reply, response) = mpsc::sync_channel::<Result<Welcome, failure::Code>>(1);
        let interrupted = contain(
            move || {
                // The reply belongs to the turn and is dropped with it.
                let _reply = reply;
                panic!("a defect met while handling a request");
            },
            &mut storm,
        );
        assert!(matches!(interrupted, Ok(None)));
        let started = Instant::now();
        assert_eq!(
            response.recv_timeout(ipc::REQUEST_TIMEOUT),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
        assert!(started.elapsed() < ipc::REQUEST_TIMEOUT / 2);
        // Turns without a panic pass through, including one that ends the
        // engine for a recorded reason.
        assert!(matches!(
            contain(|| Ok(Flow::Continue), &mut storm),
            Ok(Some(Flow::Continue))
        ));
        assert!(matches!(
            contain(|| Ok(Flow::Stop), &mut storm),
            Ok(Some(Flow::Stop))
        ));
        assert!(contain(|| anyhow::bail!("the worker cannot start"), &mut storm).is_err());
    }
    #[test]
    fn panics_that_keep_recurring_end_the_engine() {
        let start = Instant::now();
        let seconds = Duration::from_secs;
        // A defect met now and then is survived every time.
        let mut storm = Storm::default();
        for occurrence in 0..20 {
            assert!(!storm.note(start + seconds(61) * occurrence));
        }
        // More than the tolerated number inside one window is not.
        let mut storm = Storm::default();
        for occurrence in 0..Storm::TOLERATED as u32 {
            assert!(!storm.note(start + seconds(10) * occurrence));
        }
        assert!(storm.note(start + seconds(45)));
        let mut storm = Storm::default();
        let mut tolerated = 0;
        while matches!(contain(|| panic!("again"), &mut storm), Ok(None)) {
            tolerated += 1;
        }
        assert_eq!(tolerated, Storm::TOLERATED);
    }
    #[test]
    fn a_connection_that_panics_returns_its_slot_and_closes_its_session() {
        let active = Arc::new(AtomicUsize::new(0));
        let mut slots: Vec<_> = (0..Slot::LIMIT)
            .map(|_| Slot::take(&active).unwrap())
            .collect();
        assert!(Slot::take(&active).is_none());
        let (tx, rx) = mpsc::sync_channel::<Work>(4);
        let slot = slots.pop().unwrap();
        let thread = std::thread::spawn(move || {
            let _slot = slot;
            let _leaving = Leaving(&tx, 7);
            panic!("a defect met on a connection thread");
        });
        assert!(thread.join().is_err());
        assert!(matches!(rx.try_recv(), Ok(Work::Close(7))));
        assert!(Slot::take(&active).is_some());
        drop(slots);
        assert_eq!(active.load(Ordering::Acquire), 0);
    }
}
