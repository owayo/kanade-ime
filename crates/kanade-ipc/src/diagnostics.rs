//! Bounded transport, recovery and process-exit diagnostics.
//!
//! A record is a timestamp, a process ID, a predefined event name and a repeat
//! count. No API here accepts text, so input, keys, application or document
//! identifiers, paths and error messages cannot reach the file.
//!
//! `record` only queues: the calling thread performs no file I/O, and a full
//! queue drops the event. A background thread writes, and collapses a run of
//! the same event so that a failure repeated on every key cannot produce one
//! line per key.
use std::{
    io,
    path::Path,
    sync::{
        OnceLock,
        mpsc::{self, SyncSender},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Component {
    Engine,
    InputMethod,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    // Frontend transport and recovery.
    ConnectFailed,
    Reconnected,
    ConnectionClosedWhileIdle,
    ExchangeTimedOut,
    ExchangeFailed,
    EngineReportedStale,
    EngineReportedUnavailable,
    EngineRejectedRequest,
    UnexpectedReply,
    CompositionHeld,
    CompositionRestored,
    RestoreFailed,
    HeldReadingFinished,
    ReadingFinishedByRequest,
    DocumentRejectedEdit,
    // Native lifecycle, recorded only while a composition is displayed.
    ActivatedWhileComposing,
    DeactivatedWhileComposing,
    CommitRequestedWhileComposing,
    SecureInputWhileComposing,
    // Engine process.
    EngineStarted,
    EngineStopped,
    WorkerCallTimedOut,
    WorkerRequestExpired,
    WorkerCallFailed,
    ProbeCallTimedOut,
    ProbeRequestExpired,
    ProbeCallFailed,
    WorkerRestarted,
    WorkerRestartFailed,
    RequestExpiredInQueue,
    RequestAbandonedByConnection,
    SyncFailed,
    FatalAlreadyRunning,
    FatalSocketDirectory,
    FatalConfiguration,
    FatalLearningStore,
    FatalWorkerStart,
    FatalListener,
    FatalThread,
    FatalWorkerRestart,
    FatalLearningFlush,
}
impl Event {
    pub fn code(self) -> &'static str {
        match self {
            Self::ConnectFailed => "connect-failed",
            Self::Reconnected => "reconnected",
            Self::ConnectionClosedWhileIdle => "connection-closed-while-idle",
            Self::ExchangeTimedOut => "exchange-timed-out",
            Self::ExchangeFailed => "exchange-failed",
            Self::EngineReportedStale => "engine-reported-stale",
            Self::EngineReportedUnavailable => "engine-reported-unavailable",
            Self::EngineRejectedRequest => "engine-rejected-request",
            Self::UnexpectedReply => "unexpected-reply",
            Self::CompositionHeld => "composition-held",
            Self::CompositionRestored => "composition-restored",
            Self::RestoreFailed => "restore-failed",
            Self::HeldReadingFinished => "held-reading-finished",
            Self::ReadingFinishedByRequest => "reading-finished-by-request",
            Self::DocumentRejectedEdit => "document-rejected-edit",
            Self::ActivatedWhileComposing => "activated-while-composing",
            Self::DeactivatedWhileComposing => "deactivated-while-composing",
            Self::CommitRequestedWhileComposing => "commit-requested-while-composing",
            Self::SecureInputWhileComposing => "secure-input-while-composing",
            Self::EngineStarted => "engine-started",
            Self::EngineStopped => "engine-stopped",
            Self::WorkerCallTimedOut => "worker-call-timed-out",
            Self::WorkerRequestExpired => "worker-request-expired-before-send",
            Self::WorkerCallFailed => "worker-call-failed",
            Self::ProbeCallTimedOut => "probe-call-timed-out",
            Self::ProbeRequestExpired => "probe-request-expired-before-send",
            Self::ProbeCallFailed => "probe-call-failed",
            Self::WorkerRestarted => "worker-restarted",
            Self::WorkerRestartFailed => "worker-restart-failed",
            Self::RequestExpiredInQueue => "request-expired-in-queue",
            Self::RequestAbandonedByConnection => "request-abandoned-by-connection",
            Self::SyncFailed => "sync-failed",
            Self::FatalAlreadyRunning => "fatal-already-running",
            Self::FatalSocketDirectory => "fatal-socket-directory",
            Self::FatalConfiguration => "fatal-configuration",
            Self::FatalLearningStore => "fatal-learning-store",
            Self::FatalWorkerStart => "fatal-worker-start",
            Self::FatalListener => "fatal-listener",
            Self::FatalThread => "fatal-thread",
            Self::FatalWorkerRestart => "fatal-worker-restart",
            Self::FatalLearningFlush => "fatal-learning-flush",
        }
    }
}

enum Message {
    Event(Event, u64),
    Flush(SyncSender<()>),
}
static QUEUE: OnceLock<SyncSender<Message>> = OnceLock::new();

const MAX_BYTES: u64 = 64 * 1024;
/// A repeated event is written again only after this many seconds.
const REPEAT_SECONDS: u64 = 60;

/// Starts the writer for this process. `directory` replaces the per-user log
/// location, which exists only on macOS; an isolated test or development
/// instance passes its own so it cannot write into the user's log. Later calls
/// change nothing.
pub fn start(component: Component, directory: Option<&Path>) {
    let Some(path) = location(component, directory) else {
        return;
    };
    QUEUE.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<Message>(64);
        let _ = std::thread::Builder::new()
            .name("kanade-diagnostics".into())
            .spawn(move || {
                let mut run = Run::default();
                let pid = std::process::id();
                while let Ok(message) = rx.recv() {
                    let (lines, done) = match message {
                        Message::Event(event, at) => (run.accept(event, at), None),
                        Message::Flush(done) => (run.finish(), Some(done)),
                    };
                    for line in lines {
                        let _ = append(&path, &line.format(pid));
                    }
                    if let Some(done) = done {
                        let _ = done.send(());
                    }
                }
            });
        tx
    });
}
/// Queues an event without blocking. Safe on a key-handling thread.
pub fn record(event: Event) {
    if let Some(queue) = QUEUE.get() {
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let _ = queue.try_send(Message::Event(event, at));
    }
}
/// Waits, up to `limit`, for queued events to reach the file. For process exit
/// only; never call this while handling a key.
pub fn flush(limit: Duration) {
    if let Some(queue) = QUEUE.get() {
        let (done, wait) = mpsc::sync_channel(1);
        if queue.try_send(Message::Flush(done)).is_ok() {
            let _ = wait.recv_timeout(limit);
        }
    }
}

fn location(component: Component, directory: Option<&Path>) -> Option<std::path::PathBuf> {
    let directory = match directory {
        Some(directory) => directory.to_owned(),
        None if cfg!(target_os = "macos") => {
            std::path::PathBuf::from(std::env::var_os("HOME")?).join("Library/Logs/Kanade")
        }
        None => return None,
    };
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&directory).ok()?;
    Some(directory.join(match component {
        Component::Engine => "engine-events.log",
        Component::InputMethod => "input-events.log",
    }))
}

#[derive(Debug, PartialEq, Eq)]
struct Line {
    at: u64,
    event: Event,
    repeated: u32,
}
impl Line {
    fn format(&self, pid: u32) -> String {
        if self.repeated == 0 {
            format!("{} pid={pid} {}\n", self.at, self.event.code())
        } else {
            format!(
                "{} pid={pid} {} repeated={}\n",
                self.at,
                self.event.code(),
                self.repeated
            )
        }
    }
}
/// Collapses consecutive occurrences of one event.
#[derive(Default)]
struct Run {
    last: Option<(Event, u64)>,
    suppressed: u32,
    latest: u64,
}
impl Run {
    fn accept(&mut self, event: Event, at: u64) -> Vec<Line> {
        if let Some((last, written)) = self.last
            && last == event
            && at.saturating_sub(written) < REPEAT_SECONDS
        {
            self.suppressed = self.suppressed.saturating_add(1);
            self.latest = at;
            return vec![];
        }
        let mut lines = self.finish();
        lines.push(Line {
            at,
            event,
            repeated: 0,
        });
        self.last = Some((event, at));
        lines
    }
    fn finish(&mut self) -> Vec<Line> {
        let repeated = std::mem::take(&mut self.suppressed);
        match self.last {
            Some((event, _)) if repeated > 0 => vec![Line {
                at: self.latest,
                event,
                repeated,
            }],
            _ => vec![],
        }
    }
}

fn append(path: &Path, line: &str) -> io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut options = std::fs::OpenOptions::new();
    // Windows append-only handles cannot truncate a full log. The exclusive
    // lock protects both rollover and the seek/write pair from other writers.
    options.create(true).truncate(false).write(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    if file.try_lock().is_err() {
        return Ok(());
    }
    if file.metadata()?.len() >= MAX_BYTES {
        file.set_len(0)?;
    }
    file.seek(SeekFrom::End(0))?;
    file.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_run_of_one_event_is_one_line_and_one_count() {
        let mut run = Run::default();
        assert_eq!(
            run.accept(Event::ConnectFailed, 100),
            [Line {
                at: 100,
                event: Event::ConnectFailed,
                repeated: 0
            }]
        );
        for at in 100..130 {
            assert!(run.accept(Event::ConnectFailed, at).is_empty());
        }
        assert_eq!(
            run.accept(Event::Reconnected, 131),
            [
                Line {
                    at: 129,
                    event: Event::ConnectFailed,
                    repeated: 30
                },
                Line {
                    at: 131,
                    event: Event::Reconnected,
                    repeated: 0
                }
            ]
        );
        assert!(run.finish().is_empty());
    }
    #[test]
    fn a_long_run_is_written_again_after_the_repeat_interval() {
        let mut run = Run::default();
        run.accept(Event::ExchangeTimedOut, 0);
        assert!(run.accept(Event::ExchangeTimedOut, 59).is_empty());
        assert_eq!(
            run.accept(Event::ExchangeTimedOut, 60),
            [
                Line {
                    at: 59,
                    event: Event::ExchangeTimedOut,
                    repeated: 1
                },
                Line {
                    at: 60,
                    event: Event::ExchangeTimedOut,
                    repeated: 0
                }
            ]
        );
    }
    #[test]
    fn lines_hold_only_time_process_and_a_predefined_name() {
        let line = Line {
            at: 7,
            event: Event::FatalWorkerRestart,
            repeated: 2,
        };
        assert_eq!(
            line.format(42),
            "7 pid=42 fatal-worker-restart repeated=2\n"
        );
        let path = std::env::temp_dir().join(format!("kanade-events-{}", std::process::id()));
        std::fs::write(&path, vec![b'x'; MAX_BYTES as usize]).unwrap();
        append(&path, &line.format(42)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "7 pid=42 fatal-worker-restart repeated=2\n"
        );
        append(&path, &line.format(42)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "7 pid=42 fatal-worker-restart repeated=2\n".repeat(2)
        );
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn recording_without_a_writer_is_a_no_op() {
        record(Event::CompositionHeld);
        flush(Duration::from_millis(1));
    }
}
