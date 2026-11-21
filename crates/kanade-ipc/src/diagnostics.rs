//! Bounded transport, recovery and process-exit diagnostics.
//!
//! A record is a timestamp, a process ID, a predefined event name, a repeat
//! count and, for a few events, a number or a source position. No API here
//! accepts text, so input, keys, application or document identifiers, paths
//! and error messages cannot reach the file. A panic is recorded by the
//! position the compiler assigned to it; its message is never read, because a
//! message can quote the text that was being processed.
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
    /// One peer could not be accepted; the listener continues.
    ConnectionRejected,
    /// Every connection slot is taken; the new peer was turned away.
    ConnectionRefusedAtCapacity,
    ConnectionThreadFailed,
    /// No pipe instance could be offered; the listener retries.
    ListenerFailed,
    /// The engine could not let sandboxed hosts identify it. They will not
    /// connect; every other host is unaffected.
    SandboxedHostsUnreachable,
    /// A thread panicked. Carries the thread and the source position.
    Panicked,
    /// The engine dropped every session after a panic and kept running.
    RecoveredFromPanic,
    FatalAlreadyRunning,
    FatalSocketDirectory,
    FatalConfiguration,
    FatalLearningStore,
    FatalWorkerStart,
    FatalListener,
    FatalThread,
    FatalWorkerRestart,
    FatalLearningFlush,
    /// The listener could not be restored; the supervisor starts a new engine.
    FatalListenerLost,
    /// Panics kept recurring; the supervisor starts a new engine.
    FatalRepeatedPanic,
    // Engine supervisor.
    WatchStarted,
    /// The image is gone, so there is nothing left to start.
    WatchStopped,
    /// The engine process ended. Carries its exit code.
    EngineExited,
    /// The supervisor started the engine again. Carries how many times so far.
    EngineRestarted,
    EngineLaunchFailed,
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
            Self::ConnectionRejected => "connection-rejected",
            Self::ConnectionRefusedAtCapacity => "connection-refused-at-capacity",
            Self::ConnectionThreadFailed => "connection-thread-failed",
            Self::ListenerFailed => "listener-failed",
            Self::SandboxedHostsUnreachable => "sandboxed-hosts-unreachable",
            Self::Panicked => "panicked",
            Self::RecoveredFromPanic => "recovered-from-panic",
            Self::FatalAlreadyRunning => "fatal-already-running",
            Self::FatalSocketDirectory => "fatal-socket-directory",
            Self::FatalConfiguration => "fatal-configuration",
            Self::FatalLearningStore => "fatal-learning-store",
            Self::FatalWorkerStart => "fatal-worker-start",
            Self::FatalListener => "fatal-listener",
            Self::FatalThread => "fatal-thread",
            Self::FatalWorkerRestart => "fatal-worker-restart",
            Self::FatalLearningFlush => "fatal-learning-flush",
            Self::FatalListenerLost => "fatal-listener-lost",
            Self::FatalRepeatedPanic => "fatal-repeated-panic",
            Self::WatchStarted => "watch-started",
            Self::WatchStopped => "watch-stopped",
            Self::EngineExited => "engine-exited",
            Self::EngineRestarted => "engine-restarted",
            Self::EngineLaunchFailed => "engine-launch-failed",
        }
    }
}

/// The threads this project names. Any other thread is `Other`: a name set
/// elsewhere is not written to the log.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Thread {
    Main,
    Connection,
    Listener,
    WorkerTransport,
    SentenceSearch,
    Diagnostics,
    Other,
}
impl Thread {
    pub const CONNECTION: &'static str = "kanade-connection";
    pub const LISTENER: &'static str = "kanade-listener";
    pub const WORKER_TRANSPORT: &'static str = "mozc-transport";
    pub const SENTENCE_SEARCH: &'static str = "kanade-sentence-search";
    const DIAGNOSTICS: &'static str = "kanade-diagnostics";

    fn current() -> Self {
        match std::thread::current().name() {
            Some("main") => Self::Main,
            Some(Self::CONNECTION) => Self::Connection,
            Some(Self::LISTENER) => Self::Listener,
            Some(Self::WORKER_TRANSPORT) => Self::WorkerTransport,
            Some(Self::SENTENCE_SEARCH) => Self::SentenceSearch,
            Some(Self::DIAGNOSTICS) => Self::Diagnostics,
            _ => Self::Other,
        }
    }
    fn code(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Connection => "connection",
            Self::Listener => "listener",
            Self::WorkerTransport => "worker-transport",
            Self::SentenceSearch => "sentence-search",
            Self::Diagnostics => "diagnostics",
            Self::Other => "other",
        }
    }
}

const SITE_BYTES: usize = 56;
/// Where a panic started: the thread, and the source file and line that the
/// compiler recorded. Built only from `std::panic::Location`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Site {
    thread: Thread,
    file: [u8; SITE_BYTES],
    length: u8,
    line: u32,
}
impl Site {
    fn new(thread: Thread, location: &std::panic::Location<'_>) -> Self {
        let (file, length) = source_name(location.file());
        Self {
            thread,
            file,
            length,
            line: location.line(),
        }
    }
    fn file(&self) -> &str {
        // Only ASCII is stored.
        std::str::from_utf8(&self.file[..usize::from(self.length)]).unwrap_or("")
    }
}
/// Reduces a source path to its crate directory and the path inside that
/// crate. A dependency is compiled from an absolute path on the build
/// machine; nothing before its crate directory is kept. No allocation: this
/// runs inside the panic hook.
fn source_name(path: &str) -> ([u8; SITE_BYTES], u8) {
    let bytes = path.as_bytes();
    let separator = |byte: u8| byte == b'/' || byte == b'\\';
    // Component boundaries, counted from the end.
    let mut starts = [0usize; 8];
    let mut found = 0;
    let mut end = bytes.len();
    while found < starts.len() {
        let start = bytes[..end]
            .iter()
            .rposition(|&byte| separator(byte))
            .map_or(0, |index| index + 1);
        starts[found] = start;
        found += 1;
        if start == 0 {
            break;
        }
        end = start - 1;
    }
    // The crate directory is the component before the last `src`.
    let mut keep = 1;
    for index in 1..found {
        let component = &bytes[starts[index]..starts[index - 1] - 1];
        if component == b"src" && index + 1 < found {
            keep = index + 2;
            break;
        }
    }
    let mut begin = starts[keep.min(found) - 1];
    // A long path loses its leading components first.
    let mut kept = keep.min(found);
    while bytes.len() - begin > SITE_BYTES && kept > 1 {
        kept -= 1;
        begin = starts[kept - 1];
    }
    let mut name = [0u8; SITE_BYTES];
    let mut length = 0;
    for &byte in bytes[begin..].iter().take(SITE_BYTES) {
        name[length] = match byte {
            b'\\' => b'/',
            byte if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'-' | b'_') => {
                byte
            }
            _ => b'_',
        };
        length += 1;
    }
    (name, length as u8)
}

/// The one value an event may carry besides its name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Detail {
    None,
    /// A process exit code.
    ExitCode(u32),
    Count(u32),
    Site(Site),
}

enum Message {
    Event(Event, Detail, u64),
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
            .name(Thread::DIAGNOSTICS.into())
            .spawn(move || {
                let mut run = Run::default();
                let pid = std::process::id();
                while let Ok(message) = rx.recv() {
                    let (lines, done) = match message {
                        Message::Event(event, detail, at) => (run.accept(event, detail, at), None),
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
    record_with(event, Detail::None);
}
/// Queues an event with its number. Safe on a key-handling thread.
pub fn record_with(event: Event, detail: Detail) {
    if let Some(queue) = QUEUE.get() {
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let _ = queue.try_send(Message::Event(event, detail, at));
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
/// Records every panic of this process by thread and source position, and
/// waits for the record to reach the file: a panic on the main thread ends
/// the process as soon as this returns. The message is not recorded and, in
/// a release build, not printed either.
pub fn record_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(location) = info.location() {
            record_with(
                Event::Panicked,
                Detail::Site(Site::new(Thread::current(), location)),
            );
            flush(Duration::from_millis(500));
        }
        // A development build keeps the usual report on standard error.
        if cfg!(debug_assertions) {
            previous(info);
        }
    }));
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
    detail: Detail,
    repeated: u32,
}
impl Line {
    fn format(&self, pid: u32) -> String {
        use std::fmt::Write;
        let mut line = format!("{} pid={pid} {}", self.at, self.event.code());
        let _ = match self.detail {
            Detail::None => Ok(()),
            // Small codes are a program's own; large ones are system status
            // values, which are documented in hexadecimal.
            Detail::ExitCode(code) if code < 0x1_0000 => write!(line, " code={code}"),
            Detail::ExitCode(code) => write!(line, " code={code:#010x}"),
            Detail::Count(count) => write!(line, " count={count}"),
            Detail::Site(site) => write!(
                line,
                " thread={} at={}:{}",
                site.thread.code(),
                site.file(),
                site.line
            ),
        };
        if self.repeated != 0 {
            let _ = write!(line, " repeated={}", self.repeated);
        }
        line.push('\n');
        line
    }
}
/// Collapses consecutive occurrences of one event with one detail.
#[derive(Default)]
struct Run {
    last: Option<(Event, Detail, u64)>,
    suppressed: u32,
    latest: u64,
}
impl Run {
    fn accept(&mut self, event: Event, detail: Detail, at: u64) -> Vec<Line> {
        if let Some((last, last_detail, written)) = self.last
            && last == event
            && last_detail == detail
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
            detail,
            repeated: 0,
        });
        self.last = Some((event, detail, at));
        lines
    }
    fn finish(&mut self) -> Vec<Line> {
        let repeated = std::mem::take(&mut self.suppressed);
        match self.last {
            Some((event, detail, _)) if repeated > 0 => vec![Line {
                at: self.latest,
                event,
                detail,
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
    // The engine and its supervisor share this file. Another writer holds the
    // lock for one line; this runs on the writer thread, never on a key.
    let mut attempts = 0;
    while file.try_lock().is_err() {
        attempts += 1;
        if attempts == 50 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(2));
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
    fn line(at: u64, event: Event, repeated: u32) -> Line {
        Line {
            at,
            event,
            detail: Detail::None,
            repeated,
        }
    }
    #[test]
    fn a_run_of_one_event_is_one_line_and_one_count() {
        let mut run = Run::default();
        assert_eq!(
            run.accept(Event::ConnectFailed, Detail::None, 100),
            [line(100, Event::ConnectFailed, 0)]
        );
        for at in 100..130 {
            assert!(
                run.accept(Event::ConnectFailed, Detail::None, at)
                    .is_empty()
            );
        }
        assert_eq!(
            run.accept(Event::Reconnected, Detail::None, 131),
            [
                line(129, Event::ConnectFailed, 30),
                line(131, Event::Reconnected, 0)
            ]
        );
        assert!(run.finish().is_empty());
    }
    #[test]
    fn a_long_run_is_written_again_after_the_repeat_interval() {
        let mut run = Run::default();
        run.accept(Event::ExchangeTimedOut, Detail::None, 0);
        assert!(
            run.accept(Event::ExchangeTimedOut, Detail::None, 59)
                .is_empty()
        );
        assert_eq!(
            run.accept(Event::ExchangeTimedOut, Detail::None, 60),
            [
                line(59, Event::ExchangeTimedOut, 1),
                line(60, Event::ExchangeTimedOut, 0)
            ]
        );
    }
    #[test]
    fn one_event_with_different_numbers_is_not_one_run() {
        let mut run = Run::default();
        // Each exit and each restart keeps its own code and count.
        for (at, code) in [(10, 101), (11, 101), (12, 1)] {
            let lines = run.accept(Event::EngineExited, Detail::ExitCode(code), at);
            assert_eq!(lines.is_empty(), at == 11);
        }
        assert_eq!(
            run.accept(Event::EngineRestarted, Detail::Count(1), 13)
                .len(),
            1
        );
        assert_eq!(
            run.accept(Event::EngineRestarted, Detail::Count(2), 13)
                .len(),
            1
        );
    }
    #[test]
    fn lines_hold_only_time_process_and_a_predefined_name() {
        let line = line(7, Event::FatalWorkerRestart, 2);
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
    fn numbers_follow_the_event_name() {
        let format = |event, detail| {
            Line {
                at: 7,
                event,
                detail,
                repeated: 0,
            }
            .format(42)
        };
        assert_eq!(
            format(Event::EngineExited, Detail::ExitCode(101)),
            "7 pid=42 engine-exited code=101\n"
        );
        // Closing a console window ends its processes with this status.
        assert_eq!(
            format(Event::EngineExited, Detail::ExitCode(0xc000_013a)),
            "7 pid=42 engine-exited code=0xc000013a\n"
        );
        assert_eq!(
            format(Event::EngineRestarted, Detail::Count(3)),
            "7 pid=42 engine-restarted count=3\n"
        );
        let here = std::panic::Location::caller();
        assert_eq!(
            format(
                Event::Panicked,
                Detail::Site(Site::new(Thread::Connection, here))
            ),
            format!(
                "7 pid=42 panicked thread=connection at=kanade-ipc/src/diagnostics.rs:{}\n",
                here.line()
            )
        );
    }
    #[test]
    fn a_source_position_keeps_its_crate_and_nothing_before_it() {
        let name = |path: &str| {
            let (bytes, length) = source_name(path);
            String::from_utf8(bytes[..usize::from(length)].to_vec()).unwrap()
        };
        assert_eq!(
            name(r"crates\kanade-engine\src\sentence\search.rs"),
            "kanade-engine/src/sentence/search.rs"
        );
        assert_eq!(
            name("crates/kanade-engine/src/lib.rs"),
            "kanade-engine/src/lib.rs"
        );
        // A dependency is compiled from the build machine's registry.
        assert_eq!(
            name(
                r"C:\Users\builder\.cargo\registry\src\index-1949cf8c\prost-0.14.4\src\encoding.rs"
            ),
            "prost-0.14.4/src/encoding.rs"
        );
        assert_eq!(
            name("/rustc/58cf8f9c3/library/core/src/str/mod.rs"),
            "core/src/str/mod.rs"
        );
        // Without a source directory only the file name is kept.
        assert_eq!(name(r"C:\Users\builder\build.rs"), "build.rs");
        assert_eq!(name("main.rs"), "main.rs");
        assert_eq!(name(""), "");
        // Anything outside the plain path alphabet is replaced.
        assert_eq!(name("crate name/src/読み.rs"), "crate_name/src/______.rs");
        // A long path loses its crate before its file.
        let long = format!("a-crate/src/{}/deep.rs", "module".repeat(8));
        assert!(name(&long).len() <= SITE_BYTES);
        assert!(name(&long).ends_with("deep.rs"));
        let endless = "x".repeat(200);
        assert_eq!(name(&endless).len(), SITE_BYTES);
    }
    #[test]
    fn recording_without_a_writer_is_a_no_op() {
        record(Event::CompositionHeld);
        record_with(Event::EngineExited, Detail::ExitCode(1));
        flush(Duration::from_millis(1));
    }
}
