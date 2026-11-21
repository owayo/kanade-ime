// SPDX-License-Identifier: MIT
//! Only this crate knows the upstream protocol or candidate IDs.
pub mod dictionary;
use kanade_ipc::{self as ipc, Commit, Key, Preedit, Privacy, Segment, SpecialKey, Word};
use prost::Message;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, SyncSender},
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[allow(dead_code, clippy::all)]
mod mozc {
    include!("generated/mozc.rs");
    pub mod config {
        include!("generated/mozc.config.rs");
    }
    pub mod commands {
        include!("generated/mozc.commands.rs");
    }
    pub mod user_dictionary {
        include!("generated/mozc.user_dictionary.rs");
    }
}
use mozc::commands as pb;
use pb::{input::CommandType as Op, session_command::CommandType as SessionOp};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("worker transport failed")]
    Io(#[from] io::Error),
    #[error("worker deadline exceeded")]
    Timeout,
    #[error("worker returned an invalid response")]
    Protocol,
    #[error("worker version or artifact does not match its lock")]
    Version,
    #[error("candidate is no longer valid")]
    StaleCandidate,
    #[error("reading cannot be rebuilt as typed")]
    Unrestorable,
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(u64);
impl SessionId {
    /// Identifier for a scripted backend in engine tests. Real identifiers
    /// always come from the worker.
    pub const fn scripted(raw: u64) -> Self {
        Self(raw)
    }
}
#[derive(Clone, Copy, Debug)]
pub enum LearningKind {
    Conversion,
    Prediction,
}
#[derive(Clone, Debug, PartialEq)]
pub struct FocusCandidate {
    pub value: String,
    pub reading: String,
    pub annotation: String,
    pub transliteration: bool,
    /// Pinned Mozc's segment-history eraser only supports candidate index zero.
    pub history_deletable: bool,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub synthetic_preview: bool,
    pub undo_mozc_commit: bool,
    pub consumed: bool,
    pub preedit: Preedit,
    pub commit: Option<Commit>,
    pub candidates: Vec<FocusCandidate>,
    pub selected: usize,
    pub focused_segment: usize,
    pub converting: bool,
}
pub trait ConversionEngine {
    /// Process-backed engines expose their verified executable so optional searches
    /// can run in a separate worker and profile. In-process backends need no executable.
    fn probe_binary(&self) -> Option<&Path> {
        None
    }
    fn add_history(&mut self, reading: &str, value: &str) -> Result<()>;
    fn create_session(&mut self) -> Result<SessionId>;
    fn delete_session(&mut self, session: SessionId) -> Result<()>;
    fn set_privacy(&mut self, privacy: Privacy) -> Result<()>;
    fn key(&mut self, session: SessionId, key: &Key, preceding: &str) -> Result<Snapshot>;
    fn select(&mut self, session: SessionId, value: &str) -> Result<Snapshot>;
    /// Explicitly erase only a currently displayed candidate at upstream index zero.
    /// The returned snapshot reports Mozc's result; consumption is not proof of erasure.
    fn delete_candidate_history(&mut self, session: SessionId, value: &str) -> Result<Snapshot>;
    fn submit(&mut self, session: SessionId) -> Result<Snapshot>;
    fn reset(&mut self, session: SessionId) -> Result<()>;
    fn undo_learning(&mut self, session: SessionId) -> Result<()>;
    fn sync(&mut self) -> Result<()>;
    /// Bounds all calls made until the next change. `None` restores the
    /// per-call limit. An engine sets this once per request.
    fn set_deadline(&mut self, _deadline: Option<Instant>) {}
    /// Rebuilds an unconverted composition from a kana reading in an empty
    /// session. Nothing is committed or learned.
    /// False once the worker process itself failed and must be restarted. An
    /// error from a healthy worker concerns one session only.
    fn healthy(&self) -> bool {
        true
    }
    /// `Error::Unrestorable` means the session is empty and usable.
    fn restore(&mut self, _session: SessionId, _reading: &str) -> Result<Snapshot> {
        Err(Error::Unrestorable)
    }
}
struct Job {
    input: Vec<u8>,
    response: SyncSender<Result<Vec<u8>>>,
}
struct Process {
    child: Child,
    probe: bool,
    /// The process stopped answering or its pipe broke; it must be replaced.
    failed: bool,
    jobs: Option<SyncSender<Job>>,
    thread: Option<JoinHandle<()>>,
    _lock: Option<File>,
}
impl Process {
    fn spawn(binary: &Path, profile: &Path, probe: bool) -> Result<Self> {
        fs::create_dir_all(profile)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(profile, fs::Permissions::from_mode(0o700))?;
        }
        let lock = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(profile.join("kanade-worker.lock"))?;
        lock.try_lock()
            .map_err(|_| io::Error::from(io::ErrorKind::WouldBlock))?;
        let mut command = Command::new(binary);
        command
            .arg(profile)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW. The engine has no console to share, and a
            // console window opened for the worker could be closed, which
            // would end the worker.
            command.creation_flags(0x0800_0000);
        }
        let mut child = command.spawn()?;
        let mut input = child.stdin.take().ok_or(Error::Protocol)?;
        let mut output = child.stdout.take().ok_or(Error::Protocol)?;
        let (tx, rx) = mpsc::sync_channel::<Job>(1);
        let thread = std::thread::Builder::new()
            .name(ipc::diagnostics::Thread::WORKER_TRANSPORT.into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    let result = ipc::write_frame(&mut input, &job.input)
                        .and_then(|()| ipc::read_frame(&mut output))
                        .map_err(Error::Io);
                    let failed = result.is_err();
                    let _ = job.response.send(result);
                    if failed {
                        break;
                    }
                }
            })?;
        Ok(Self {
            child,
            probe,
            failed: false,
            jobs: Some(tx),
            thread: Some(thread),
            _lock: Some(lock),
        })
    }
    fn call(&mut self, input: pb::Input, timeout: Duration) -> Result<pb::Output> {
        let output = self.exchange(input.encode_to_vec(), timeout)?;
        pb::Output::decode(output.as_slice()).map_err(|_| Error::Protocol)
    }
    fn stop(&mut self, event: ipc::diagnostics::Event) -> Error {
        ipc::diagnostics::record(event);
        self.failed = true;
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.jobs = None;
        Error::Timeout
    }
    fn exchange(&mut self, input: Vec<u8>, timeout: Duration) -> Result<Vec<u8>> {
        use ipc::diagnostics::{Event, record};
        if timeout.is_zero() {
            // No call is in flight. Other input sessions can keep their state.
            if self.probe {
                // An interrupted probe can own a temporary session awaiting deletion.
                return Err(self.stop(Event::ProbeRequestExpired));
            }
            record(Event::WorkerRequestExpired);
            return Err(Error::Timeout);
        }
        let (tx, rx) = mpsc::sync_channel(1);
        self.jobs
            .as_ref()
            .ok_or(Error::Protocol)?
            .try_send(Job {
                input,
                response: tx,
            })
            .map_err(|_| Error::Protocol)?;
        match rx.recv_timeout(timeout) {
            Ok(result) => {
                if result.is_err() {
                    record(if self.probe {
                        Event::ProbeCallFailed
                    } else {
                        Event::WorkerCallFailed
                    });
                    self.failed = true;
                }
                result
            }
            Err(_) => Err(self.stop(if self.probe {
                Event::ProbeCallTimedOut
            } else {
                Event::WorkerCallTimedOut
            })),
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.jobs.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self._lock.take();
    }
}

#[derive(serde::Deserialize)]
struct Manifest {
    worker_protocol: u32,
    mozc_commit: String,
    tag: String,
    sha256: String,
}
/// The pinned upstream release and the verified worker executable. The
/// executable embeds its dictionary, so the digest also identifies the data.
/// The release fields come from the lock the manifest was checked against;
/// the worker is not asked for them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerIdentity {
    pub tag: String,
    pub commit: String,
    pub data_version: u32,
    pub engine_version: u32,
    pub sha256: String,
    pub protocol: u32,
}
pub fn verify_artifact(binary: &Path) -> Result<()> {
    worker_identity(binary).map(drop)
}
fn worker_identity(binary: &Path) -> Result<WorkerIdentity> {
    let adjacent = binary.with_file_name("manifest.json");
    let manifest_path = if adjacent.is_file() {
        adjacent
    } else {
        binary
            .parent()
            .and_then(Path::parent)
            .ok_or(Error::Version)?
            .join("Resources/manifest.json")
    };
    let manifest: Manifest =
        serde_json::from_slice(&fs::read(manifest_path)?).map_err(|_| Error::Version)?;
    let lock: toml::Value = toml::from_str(include_str!("../../../locks/mozc.lock.toml"))
        .map_err(|_| Error::Version)?;
    if lock["mozc"]["commit"].as_str() != Some(manifest.mozc_commit.as_str())
        || manifest.worker_protocol != 2
        || lock["mozc"]["tag"].as_str() != Some(manifest.tag.as_str())
        || artifact_hash(binary)? != manifest.sha256
    {
        return Err(Error::Version);
    }
    let pinned = |name: &str| {
        lock["mozc"]
            .get(name)
            .and_then(toml::Value::as_integer)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or(Error::Version)
    };
    Ok(WorkerIdentity {
        data_version: pinned("data_version")?,
        engine_version: pinned("engine_version")?,
        tag: manifest.tag,
        commit: manifest.mozc_commit,
        sha256: manifest.sha256,
        protocol: manifest.worker_protocol,
    })
}
fn artifact_hash(path: &Path) -> io::Result<String> {
    use std::io::Read;
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let len = file.read(&mut buffer)?;
        if len == 0 {
            break;
        }
        digest.update(&buffer[..len]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
pub fn default_worker() -> PathBuf {
    if let Some(path) = std::env::var_os("KANADE_MOZC_WORKER") {
        return path.into();
    }
    let name = if cfg!(windows) {
        "kanade-mozc-worker.exe"
    } else {
        "kanade-mozc-worker"
    };
    let packaged = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join(name)));
    if let Some(path) = packaged.filter(|p| p.is_file()) {
        return path;
    }
    if let Ok(executable) = std::env::current_exe() {
        for parent in executable.ancestors().skip(1).take(5) {
            let candidate = parent.join("artifacts/mozc").join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from("artifacts/mozc").join(name)
}
#[derive(Default)]
struct Session {
    last: Snapshot,
    candidates: Vec<pb::CandidateWord>,
}
pub struct Mozc {
    process: Process,
    sessions: HashMap<SessionId, Session>,
    privacy: Option<Privacy>,
    binary: PathBuf,
    profile: PathBuf,
    deadline: Option<Instant>,
    identity: WorkerIdentity,
    request_timeout: Duration,
    suggestions: bool,
}
impl Mozc {
    pub fn reload_dictionary(&mut self, generation: u64) -> Result<dictionary::Reload> {
        if generation == 0 {
            return Err(Error::Protocol);
        }
        self.dictionary_control(1, generation)
    }
    pub fn dictionary_reload_status(&mut self) -> Result<dictionary::Reload> {
        self.dictionary_control(2, 0)
    }
    fn dictionary_control(&mut self, operation: u8, generation: u64) -> Result<dictionary::Reload> {
        const PREFIX: &[u8] = b"\0KANADE\x01";
        let mut input = PREFIX.to_vec();
        input.push(operation);
        input.extend_from_slice(&generation.to_le_bytes());
        let limit = self.limit().min(ipc::WORKER_TIMEOUT);
        let output = self.process.exchange(input, limit)?;
        if output.len() != PREFIX.len() + 9 || !output.starts_with(PREFIX) {
            return Err(Error::Protocol);
        }
        let state = match output[PREFIX.len()] {
            0 => dictionary::ReloadState::Idle,
            1 => dictionary::ReloadState::Loading,
            2 => dictionary::ReloadState::Ready,
            3 => dictionary::ReloadState::Failed,
            4 => dictionary::ReloadState::Busy,
            _ => return Err(Error::Protocol),
        };
        let generation = u64::from_le_bytes(
            output[PREFIX.len() + 1..]
                .try_into()
                .map_err(|_| Error::Protocol)?,
        );
        Ok(dictionary::Reload { generation, state })
    }
    /// Explicit administration; never called while a document edit is pending.
    pub fn clear_learning(&mut self, kind: LearningKind) -> Result<()> {
        self.call(pb::Input {
            r#type: match kind {
                LearningKind::Conversion => Op::ClearUserHistory,
                LearningKind::Prediction => Op::ClearUserPrediction,
            } as i32,
            ..Default::default()
        })?;
        self.sync()
    }
    pub fn start(binary: &Path, profile: &Path) -> Result<Self> {
        Self::start_worker(binary, profile, false)
    }
    /// Start optional sentence exploration in its own disposable profile.
    pub fn start_probe(binary: &Path, profile: &Path) -> Result<Self> {
        Self::start_worker(binary, profile, true)
    }
    fn start_worker(binary: &Path, profile: &Path, probe: bool) -> Result<Self> {
        let identity = worker_identity(binary)?;
        let mut process = Process::spawn(binary, profile, probe)?;
        let output = process.call(
            pb::Input {
                r#type: Op::GetServerVersion as i32,
                ..Default::default()
            },
            Duration::from_secs(30),
        )?;
        let version = output
            .server_version
            .and_then(|v| v.mozc_version)
            .ok_or(Error::Version)?;
        let expected: toml::Value = toml::from_str(include_str!("../../../locks/mozc.lock.toml"))
            .map_err(|_| Error::Version)?;
        if expected["mozc"]["tag"]
            .as_str()
            .is_none_or(|tag| !version.starts_with(tag))
        {
            return Err(Error::Version);
        }
        Ok(Self {
            process,
            sessions: HashMap::new(),
            privacy: None,
            binary: binary.to_owned(),
            profile: profile.to_owned(),
            deadline: None,
            identity,
            request_timeout: ipc::WORKER_TIMEOUT,
            suggestions: true,
        })
    }
    /// The artifact verified when this worker was started.
    pub fn identity(&self) -> &WorkerIdentity {
        &self.identity
    }
    /// Configure ordinary offline request waits. An active input-event deadline
    /// still takes precedence, so the synchronous key budget is never extended.
    /// Startup and dictionary control retain their separate limits.
    pub fn set_request_timeout(&mut self, timeout: Duration) {
        self.request_timeout = timeout;
    }
    /// Whether later keys ask the worker for suggestions while composing.
    /// Each suggestion reads the whole composition, so typing a long reading
    /// costs far more than its length. Offline evaluation, which only
    /// converts, turns them off: neither the composition nor its conversion
    /// uses them. Interactive input keeps the default.
    pub fn set_suggestions(&mut self, enabled: bool) {
        self.suggestions = enabled;
    }
    /// Stop and reap the previous process before opening the profile again.
    pub fn restart(&mut self) -> Result<()> {
        self.process.jobs.take();
        let _ = self.process.child.kill();
        let _ = self.process.child.wait();
        if let Some(thread) = self.process.thread.take() {
            let _ = thread.join();
        }
        self.process._lock.take();
        let mut fresh = Self::start_worker(&self.binary, &self.profile, self.process.probe)?;
        fresh.request_timeout = self.request_timeout;
        fresh.suggestions = self.suggestions;
        *self = fresh;
        self.set_privacy(Privacy::Unknown)
    }
    pub fn terminate(&mut self) -> Result<()> {
        self.process.child.kill()?;
        self.process.child.wait()?;
        Ok(())
    }
    /// Time left for a call. The limit detects a worker that stopped
    /// answering; a slow answer inside it is still an answer. Exceeding it
    /// kills the worker, which ends every session, so it is not a latency goal.
    fn limit(&self) -> Duration {
        self.deadline.map_or(self.request_timeout, |deadline| {
            deadline
                .saturating_duration_since(Instant::now())
                .min(self.request_timeout)
        })
    }
    fn call(&mut self, input: pb::Input) -> Result<pb::Output> {
        let session = input.id;
        let limit = self.limit();
        let output = self.process.call(input, limit)?;
        if output.error_code.unwrap_or(0) != 0 || (session.is_some() && output.id == Some(0)) {
            return Err(Error::Protocol);
        }
        Ok(output)
    }
    fn command(
        &mut self,
        id: SessionId,
        op: SessionOp,
        candidate: Option<i32>,
    ) -> Result<Snapshot> {
        let output = self.call(pb::Input {
            r#type: Op::SendCommand as i32,
            id: Some(id.0),
            command: Some(pb::SessionCommand {
                r#type: op as i32,
                id: candidate,
                ..Default::default()
            }),
            ..Default::default()
        })?;
        self.snapshot(id, output)
    }
    fn snapshot(&mut self, id: SessionId, output: pb::Output) -> Result<Snapshot> {
        let session = self.sessions.get_mut(&id).ok_or(Error::Protocol)?;
        if let Some(ref candidates) = output.all_candidate_words {
            session.candidates = candidates.candidates.clone();
        }
        if output.preedit.is_none() && output.candidate_window.is_none() {
            session.candidates.clear();
        }
        let preedit = output
            .preedit
            .as_ref()
            .map(|p| {
                let text: String = p.segment.iter().map(|s| s.value.as_str()).collect();
                Preedit {
                    cursor_utf16: ipc::scalar_to_utf16(&text, p.cursor as usize),
                    segments: p
                        .segment
                        .iter()
                        .map(|s| Segment {
                            reading: s.key.clone().unwrap_or_else(|| s.value.clone()),
                            value: s.value.clone(),
                            focused: s.annotation == 2,
                            value_utf16: ipc::utf16_len(&s.value),
                        })
                        .collect(),
                }
            })
            .unwrap_or_default();
        let undo_mozc_commit = output.result.is_some();
        let commit = output.result.map(|r| Commit {
            reading: r.key.unwrap_or_default(),
            value: r.value,
            words: r
                .tokens
                .iter()
                .map(|t| Word {
                    reading: t.key.clone().unwrap_or_default(),
                    value: t.value.clone().unwrap_or_default(),
                })
                .collect(),
            synthetic: false,
            transient: false,
        });
        let candidates = session
            .candidates
            .iter()
            .map(|c| FocusCandidate {
                value: c.value.clone().unwrap_or_default(),
                reading: c.key.clone().unwrap_or_else(|| {
                    preedit
                        .segments
                        .iter()
                        .find(|s| s.focused)
                        .map(|s| s.reading.clone())
                        .unwrap_or_default()
                }),
                annotation: c
                    .annotation
                    .as_ref()
                    .and_then(|a| a.description.clone())
                    .unwrap_or_default(),
                transliteration: c.id.is_some_and(|id| id < 0),
                history_deletable: c.id == Some(0),
            })
            .collect();
        let selected = output
            .candidate_window
            .as_ref()
            .and_then(|w| w.focused_index)
            .or_else(|| {
                output
                    .all_candidate_words
                    .as_ref()
                    .and_then(|w| w.focused_index)
            })
            .unwrap_or(0) as usize;
        let snapshot = Snapshot {
            synthetic_preview: false,
            undo_mozc_commit,
            consumed: output.consumed.unwrap_or(false),
            focused_segment: preedit.segments.iter().position(|s| s.focused).unwrap_or(0),
            converting: preedit.segments.iter().any(|s| s.focused),
            preedit,
            commit,
            candidates,
            selected,
        };
        session.last = snapshot.clone();
        Ok(snapshot)
    }
    /// Flush before reload: reloading first can discard unsaved Mozc learning.
    pub fn reload(&mut self) -> Result<()> {
        self.sync()?;
        self.call(pb::Input {
            r#type: Op::Reload as i32,
            ..Default::default()
        })?;
        Ok(())
    }
    pub fn add_history(&mut self, reading: &str, value: &str) -> Result<()> {
        self.call(pb::Input {
            r#type: Op::AddUserHistory as i32,
            user_history_data: Some(pb::UserHistoryData {
                key: Some(reading.to_owned()),
                value: Some(value.to_owned()),
            }),
            ..Default::default()
        })?;
        Ok(())
    }
    pub fn shutdown(&mut self) -> Result<()> {
        self.sync()?;
        self.call(pb::Input {
            r#type: Op::Shutdown as i32,
            ..Default::default()
        })?;
        Ok(())
    }
}
impl ConversionEngine for Mozc {
    fn probe_binary(&self) -> Option<&Path> {
        Some(&self.binary)
    }
    fn add_history(&mut self, reading: &str, value: &str) -> Result<()> {
        Mozc::add_history(self, reading, value)
    }
    fn create_session(&mut self) -> Result<SessionId> {
        if self.sessions.len() >= 32 {
            return Err(Error::Protocol);
        }
        let out = self.call(pb::Input {
            r#type: Op::CreateSession as i32,
            ..Default::default()
        })?;
        let id = SessionId(out.id.filter(|id| *id != 0).ok_or(Error::Protocol)?);
        self.sessions.insert(id, Session::default());
        Ok(id)
    }
    fn delete_session(&mut self, id: SessionId) -> Result<()> {
        self.call(pb::Input {
            r#type: Op::DeleteSession as i32,
            id: Some(id.0),
            ..Default::default()
        })?;
        self.sessions.remove(&id);
        Ok(())
    }
    fn set_privacy(&mut self, privacy: Privacy) -> Result<()> {
        if self.privacy == Some(privacy) {
            return Ok(());
        }
        let out = self.call(pb::Input {
            r#type: Op::GetConfig as i32,
            ..Default::default()
        })?;
        let mut config = out.config.ok_or(Error::Protocol)?;
        config.session_keymap = Some(mozc::config::config::SessionKeymap::Msime as i32);
        config.history_learning_level = Some(if privacy == Privacy::Normal { 0 } else { 1 });
        config.incognito_mode = Some(privacy != Privacy::Normal);
        self.call(pb::Input {
            r#type: Op::SetConfig as i32,
            config: Some(config),
            ..Default::default()
        })?;
        self.privacy = Some(privacy);
        Ok(())
    }
    fn key(&mut self, id: SessionId, key: &Key, preceding: &str) -> Result<Snapshot> {
        if key.special == SpecialKey::None as i32 && key.modifiers & ipc::MOD_META != 0 {
            // The worker has no Command modifier. Without it the chord would
            // be typed into the composition as a letter.
            let session = self.sessions.get(&id).ok_or(Error::Protocol)?;
            return Ok(Snapshot {
                consumed: false,
                commit: None,
                undo_mozc_commit: false,
                ..session.last.clone()
            });
        }
        let special = match SpecialKey::try_from(key.special).map_err(|_| Error::Protocol)? {
            SpecialKey::None => None,
            SpecialKey::Space => Some(pb::key_event::SpecialKey::Space),
            SpecialKey::Enter => Some(pb::key_event::SpecialKey::Enter),
            SpecialKey::Backspace => Some(pb::key_event::SpecialKey::Backspace),
            SpecialKey::Escape => Some(pb::key_event::SpecialKey::Escape),
            SpecialKey::Left => Some(pb::key_event::SpecialKey::Left),
            SpecialKey::Right => Some(pb::key_event::SpecialKey::Right),
            SpecialKey::Up => Some(pb::key_event::SpecialKey::Up),
            SpecialKey::Down => Some(pb::key_event::SpecialKey::Down),
            SpecialKey::Tab => Some(pb::key_event::SpecialKey::Tab),
            SpecialKey::Delete => Some(pb::key_event::SpecialKey::Del),
            SpecialKey::Home => Some(pb::key_event::SpecialKey::Home),
            SpecialKey::End => Some(pb::key_event::SpecialKey::End),
        };
        let mut event = pb::KeyEvent {
            special_key: special.map(|s| s as i32),
            activated: Some(true),
            mode: Some(pb::CompositionMode::Hiragana as i32),
            ..Default::default()
        };
        if special.is_none() {
            if key.text.chars().count() == 1 && key.text.is_ascii() {
                event.key_code = key.text.chars().next().map(|c| c as u32);
            } else {
                event.key_string = Some(key.text.clone());
            }
        }
        let modifiers = worker_modifiers(key);
        for (mask, modifier) in [
            (ipc::MOD_SHIFT, pb::key_event::ModifierKey::Shift),
            (ipc::MOD_CONTROL, pb::key_event::ModifierKey::Ctrl),
            (ipc::MOD_ALT, pb::key_event::ModifierKey::Alt),
        ] {
            if modifiers & mask != 0 {
                event.modifier_keys.push(modifier as i32);
            }
        }
        let out = self.call(pb::Input {
            r#type: Op::SendKey as i32,
            id: Some(id.0),
            key: Some(event),
            context: Some(pb::Context {
                preceding_text: Some(preceding.to_owned()),
                ..Default::default()
            }),
            // Left unset unless suggestions were turned off, so interactive
            // requests are sent exactly as before.
            request_suggestion: (!self.suggestions).then_some(false),
            ..Default::default()
        })?;
        self.snapshot(id, out)
    }
    fn select(&mut self, id: SessionId, value: &str) -> Result<Snapshot> {
        let candidate = self
            .sessions
            .get(&id)
            .and_then(|s| {
                s.candidates
                    .iter()
                    .find(|c| c.value.as_deref() == Some(value))
            })
            .and_then(|c| c.id)
            .ok_or(Error::StaleCandidate)?;
        self.command(id, SessionOp::SelectCandidate, Some(candidate))
    }
    fn submit(&mut self, id: SessionId) -> Result<Snapshot> {
        self.command(id, SessionOp::Submit, None)
    }
    fn delete_candidate_history(&mut self, id: SessionId, value: &str) -> Result<Snapshot> {
        let candidate = self
            .sessions
            .get(&id)
            .filter(|s| s.last.converting)
            .and_then(|s| {
                s.candidates
                    .iter()
                    .find(|c| c.value.as_deref() == Some(value))
            })
            .and_then(|c| c.id)
            // The upstream segment-history rewriter ignores candidate_index and
            // reads segment.candidate(0). Reject other IDs rather than erasing a
            // different word's history while processing the requested candidate.
            .filter(|id| *id == 0)
            .ok_or(Error::StaleCandidate)?;
        self.command(id, SessionOp::DeleteCandidateFromHistory, Some(candidate))
    }
    fn reset(&mut self, id: SessionId) -> Result<()> {
        self.command(id, SessionOp::ResetContext, None)?;
        Ok(())
    }
    fn undo_learning(&mut self, id: SessionId) -> Result<()> {
        self.command(id, SessionOp::Revert, None)?;
        Ok(())
    }
    fn sync(&mut self) -> Result<()> {
        self.call(pb::Input {
            r#type: Op::SyncData as i32,
            ..Default::default()
        })?;
        Ok(())
    }
    fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
    }
    fn healthy(&self) -> bool {
        !self.process.failed
    }
    fn restore(&mut self, id: SessionId, reading: &str) -> Result<Snapshot> {
        if reading.is_empty()
            || self
                .sessions
                .get(&id)
                .is_none_or(|session| !session.last.preedit.segments.is_empty())
        {
            return Err(Error::Protocol);
        }
        // The worker reports romaji that is still waiting for its vowel as
        // fullwidth Latin at the end of the reading. Kana before it is inserted
        // as already composed text; the waiting letters are typed again, so
        // they keep combining with the next key. Text inserted as a string
        // would stay a literal.
        let pending = reading
            .chars()
            .rev()
            .take_while(|c| ('ａ'..='ｚ').contains(c))
            .count();
        if pending > 8 {
            return Err(Error::Protocol);
        }
        let split = reading
            .char_indices()
            .rev()
            .nth(pending.wrapping_sub(1))
            .map_or(reading.len(), |(index, _)| index);
        let (kana, letters) = if pending == 0 {
            (reading, "")
        } else {
            reading.split_at(split)
        };
        let mut restored = Ok(Snapshot::default());
        if !kana.is_empty() {
            restored = self
                .call(pb::Input {
                    r#type: Op::SendKey as i32,
                    id: Some(id.0),
                    key: Some(pb::KeyEvent {
                        key_string: Some(kana.to_owned()),
                        activated: Some(true),
                        mode: Some(pb::CompositionMode::Hiragana as i32),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .and_then(|out| self.snapshot(id, out));
        }
        for letter in letters.chars() {
            if restored.is_err() {
                break;
            }
            let ascii =
                char::from_u32(letter as u32 - 'ａ' as u32 + 'a' as u32).ok_or(Error::Protocol)?;
            restored = self.key(
                id,
                &Key {
                    text: ascii.into(),
                    ..Default::default()
                },
                "",
            );
        }
        match restored {
            Ok(snapshot)
                if snapshot.commit.is_none()
                    && !snapshot.converting
                    && ipc::reading(&snapshot.preedit) == reading =>
            {
                Ok(snapshot)
            }
            // A dead worker is the caller's failure to handle.
            Err(error @ (Error::Timeout | Error::Io(_))) => Err(error),
            // The worker changed the text. Leave the session empty instead of
            // continuing from a composition the user did not type.
            _ => {
                self.reset(id)?;
                Err(Error::Unrestorable)
            }
        }
    }
}

fn worker_modifiers(key: &Key) -> u32 {
    // Symbol text already includes the keyboard layout and Shift result.
    // Preserve the existing shifted alphanumeric and shortcut behavior.
    if key.special == SpecialKey::None as i32
        && key.modifiers == ipc::MOD_SHIFT
        && key.text.len() == 1
        && key.text.as_bytes()[0].is_ascii_punctuation()
    {
        0
    } else {
        key.modifiers
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;
    #[test]
    fn printable_shift_preserves_shortcuts_and_special_keys() {
        for text in ["~", "!", "?", "^", "@", "_", "|"] {
            let key = Key {
                text: text.into(),
                modifiers: ipc::MOD_SHIFT,
                ..Default::default()
            };
            assert_eq!(worker_modifiers(&key), 0);
            for other in [ipc::MOD_CONTROL, ipc::MOD_ALT, ipc::MOD_META] {
                let key = Key {
                    modifiers: ipc::MOD_SHIFT | other,
                    ..key.clone()
                };
                assert_eq!(worker_modifiers(&key), key.modifiers);
            }
        }
        for text in ["", " ", "〜", "ab", "A", "a", "0"] {
            let key = Key {
                text: text.into(),
                modifiers: ipc::MOD_SHIFT,
                ..Default::default()
            };
            assert_eq!(worker_modifiers(&key), ipc::MOD_SHIFT);
        }
        for special in [
            SpecialKey::Space,
            SpecialKey::Left,
            SpecialKey::Enter,
            SpecialKey::Tab,
        ] {
            let key = Key {
                special: special as i32,
                modifiers: ipc::MOD_SHIFT,
                ..Default::default()
            };
            assert_eq!(worker_modifiers(&key), ipc::MOD_SHIFT);
        }
    }
}

/// A stand-in worker replays prepared replies with scripted pauses, so a slow
/// reply and a silent worker are reproduced without the real conversion engine.
#[cfg(all(test, unix))]
mod deadline_tests {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, time::Instant};

    fn frame(output: &pb::Output) -> Vec<u8> {
        let mut bytes = Vec::new();
        ipc::write_frame(&mut bytes, &output.encode_to_vec()).unwrap();
        bytes
    }
    /// `steps` are shell lines; `reply N` emits the Nth prepared reply.
    fn worker(directory: &Path, replies: &[pb::Output], steps: &str) -> PathBuf {
        let lock: toml::Value =
            toml::from_str(include_str!("../../../locks/mozc.lock.toml")).unwrap();
        for (index, reply) in replies.iter().enumerate() {
            fs::write(directory.join(format!("reply{index}")), frame(reply)).unwrap();
        }
        let binary = directory.join("kanade-mozc-worker");
        fs::write(
            &binary,
            format!(
                "#!/bin/sh\nreply() {{ cat \"$(dirname \"$0\")/reply$1\"; }}\nreply 0\n{steps}\nexec sleep 30\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            directory.join("manifest.json"),
            serde_json::json!({
                "worker_protocol": 2,
                "mozc_commit": lock["mozc"]["commit"].as_str().unwrap(),
                "tag": lock["mozc"]["tag"].as_str().unwrap(),
                "sha256": artifact_hash(&binary).unwrap(),
            })
            .to_string(),
        )
        .unwrap();
        binary
    }
    fn version() -> pb::Output {
        let lock: toml::Value =
            toml::from_str(include_str!("../../../locks/mozc.lock.toml")).unwrap();
        pb::Output {
            server_version: Some(pb::output::VersionInfo {
                mozc_version: Some(format!("{}.0", lock["mozc"]["tag"].as_str().unwrap())),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
    fn session(id: u64) -> pb::Output {
        pb::Output {
            id: Some(id),
            ..Default::default()
        }
    }

    #[test]
    fn a_slow_reply_is_delivered_and_the_worker_survives() {
        let temp = tempfile::tempdir().unwrap();
        let binary = worker(
            temp.path(),
            &[version(), session(7), session(8)],
            "sleep 0.12\nreply 1\nreply 2",
        );
        let mut mozc = Mozc::start(&binary, &temp.path().join("profile")).unwrap();
        // 120 ms is slow, not dead: page-ins, disk syncs and CPU contention
        // reach it. Treating it as a failure ends every open composition.
        assert_eq!(mozc.create_session().unwrap(), SessionId(7));
        assert_eq!(mozc.create_session().unwrap(), SessionId(8));
    }
    #[test]
    fn an_unresponsive_probe_does_not_end_the_live_session() {
        let temp = tempfile::tempdir().unwrap();
        let binary = worker(
            temp.path(),
            &[version(), session(7), session(8)],
            "case \"$1\" in */live) reply 1; reply 2;; esac",
        );
        let mut live = Mozc::start(&binary, &temp.path().join("live")).unwrap();
        let first = live.create_session().unwrap();
        let mut probe = Mozc::start_probe(&binary, &temp.path().join("search")).unwrap();
        probe.set_deadline(Some(Instant::now() + Duration::from_millis(60)));
        assert!(matches!(probe.create_session(), Err(Error::Timeout)));
        assert!(!probe.healthy());
        assert!(live.healthy());
        assert!(live.sessions.contains_key(&first));
        assert_ne!(live.create_session().unwrap(), first);
    }

    #[test]
    fn a_silent_worker_is_stopped_at_the_request_deadline() {
        let temp = tempfile::tempdir().unwrap();
        let binary = worker(temp.path(), &[version()], "");
        let mut mozc = Mozc::start(&binary, &temp.path().join("profile")).unwrap();
        let started = Instant::now();
        mozc.set_deadline(Some(started + Duration::from_millis(60)));
        assert!(matches!(mozc.create_session(), Err(Error::Timeout)));
        assert!(started.elapsed() < ipc::WORKER_TIMEOUT / 2);
        // The stopped worker is not asked again; the caller must restart it.
        mozc.set_deadline(None);
        let retried = Instant::now();
        assert!(mozc.create_session().is_err());
        assert!(retried.elapsed() < Duration::from_millis(100));
    }
    #[test]
    fn an_expired_deadline_never_reaches_the_worker() {
        let temp = tempfile::tempdir().unwrap();
        let binary = worker(temp.path(), &[version(), session(7)], "reply 1");
        let mut mozc = Mozc::start(&binary, &temp.path().join("profile")).unwrap();
        mozc.set_deadline(Some(Instant::now()));
        assert!(matches!(mozc.create_session(), Err(Error::Timeout)));
        mozc.set_deadline(None);
        assert!(mozc.healthy());
        assert_eq!(mozc.create_session().unwrap(), SessionId(7));
    }
    #[test]
    fn restart_succeeds_when_the_fresh_worker_answers_slowly() {
        let temp = tempfile::tempdir().unwrap();
        let configured = pb::Output {
            config: Some(Default::default()),
            ..Default::default()
        };
        // The first process goes silent. Its replacement is slow to answer the
        // configuration calls made during restart, as a cold process is.
        let binary = worker(
            temp.path(),
            &[version(), configured, session(1), session(9)],
            "marker=\"$(dirname \"$0\")/started\"\n\
             if [ -e \"$marker\" ]; then sleep 0.12; reply 1; reply 2; reply 3; fi\n\
             : > \"$marker\"",
        );
        let mut mozc = Mozc::start(&binary, &temp.path().join("profile")).unwrap();
        mozc.set_deadline(Some(Instant::now() + Duration::from_millis(60)));
        assert!(matches!(mozc.create_session(), Err(Error::Timeout)));
        mozc.set_deadline(None);
        mozc.restart().unwrap();
        assert_eq!(mozc.create_session().unwrap(), SessionId(9));
    }
}

#[cfg(test)]
mod evaluation_settings_tests {
    use super::*;
    #[test]
    #[ignore = "requires the locked worker; make contract"]
    fn an_expired_request_keeps_another_session_composing() {
        let profile = tempfile::tempdir().unwrap();
        let mut mozc = Mozc::start(&default_worker(), profile.path()).unwrap();
        let live = mozc.create_session().unwrap();
        mozc.restore(live, "あした").unwrap();
        mozc.set_deadline(Some(Instant::now()));
        assert!(matches!(mozc.create_session(), Err(Error::Timeout)));
        mozc.set_deadline(None);
        let space = Key {
            special: ipc::SpecialKey::Space as i32,
            ..Default::default()
        };
        let preview = mozc.key(live, &space, "").unwrap();
        assert!(preview.converting);
        assert_eq!(ipc::reading(&preview.preedit), "あした");
        mozc.delete_session(live).unwrap();
    }
    #[test]
    #[ignore = "requires the locked worker; make contract"]
    fn offline_limits_preserve_the_input_deadline_and_survive_restart() {
        let profile = tempfile::tempdir().unwrap();
        let mut mozc = Mozc::start(&default_worker(), profile.path()).unwrap();
        let identity = mozc.identity().clone();
        assert_eq!(mozc.limit(), ipc::WORKER_TIMEOUT);
        mozc.set_request_timeout(Duration::from_secs(10));
        assert_eq!(mozc.limit(), Duration::from_secs(10));
        mozc.set_deadline(Some(Instant::now() + Duration::from_millis(400)));
        assert!(mozc.limit() <= Duration::from_millis(400));
        mozc.set_deadline(Some(Instant::now()));
        mozc.set_request_timeout(Duration::from_secs(20));
        assert_eq!(mozc.limit(), Duration::ZERO);
        mozc.set_deadline(None);
        mozc.set_suggestions(false);
        mozc.restart().unwrap();
        assert_eq!(mozc.identity(), &identity);
        assert_eq!(mozc.limit(), Duration::from_secs(20));
        assert!(!mozc.suggestions);
        mozc.shutdown().unwrap();
    }
}
