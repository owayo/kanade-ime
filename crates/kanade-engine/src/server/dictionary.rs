//! Explicit management work; no vocabulary is kept in job diagnostics.
use super::*;
use kanade_mozc::dictionary::{self as storage, Edit, Prepared, Reload, ReloadState};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Mutex, atomic::AtomicU8},
};

const CANCELLABLE: u8 = 0;
const PUBLISHING: u8 = 1;
const CANCELLED: u8 = 2;
const POLL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Stage {
    Queued,
    Preparing,
    WaitingForIdle,
    Loading,
    Ready,
    Failed,
    Cancelled,
}
impl Stage {
    fn finished(self) -> bool {
        matches!(self, Self::Ready | Self::Failed | Self::Cancelled)
    }
}
#[derive(Clone, Serialize)]
pub(super) struct Status {
    id: String,
    state: Stage,
    saved_revision: Option<String>,
    reason: Option<&'static str>,
}
struct Slot {
    status: Status,
    permit: Arc<AtomicU8>,
}
struct State {
    next: u64,
    jobs: BTreeMap<u64, Slot>,
}
pub(super) struct Service {
    state: Arc<Mutex<State>>,
    tx: SyncSender<Job>,
}
struct Job {
    id: u64,
    edit: Edit,
    permit: Arc<AtomicU8>,
}
impl Service {
    pub(super) fn start(profile: PathBuf, engine: SyncSender<Work>) -> io::Result<Self> {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let state = Arc::new(Mutex::new(State {
            next: seed.max(1),
            jobs: BTreeMap::new(),
        }));
        let (tx, rx) = mpsc::sync_channel::<Job>(1);
        let shared = state.clone();
        std::thread::Builder::new()
            .name("kanade-dictionary".into())
            .spawn(move || {
                let mut generation = seed.max(1);
                while let Ok(job) = rx.recv() {
                    run(job, &profile, &engine, &shared, &mut generation);
                }
            })?;
        Ok(Self { state, tx })
    }
    pub(super) fn submit(&self, edit: Edit) -> io::Result<Status> {
        let mut state = self.state.lock().map_err(|_| io::ErrorKind::Other)?;
        // One unfinished request bounds memory and serializes the disk revision.
        if state
            .jobs
            .values()
            .any(|slot| !slot.status.state.finished())
        {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let id = state.next.checked_add(1).ok_or(io::ErrorKind::Other)?;
        state.next = id;
        let status = Status {
            id: id.to_string(),
            state: Stage::Queued,
            saved_revision: None,
            reason: None,
        };
        let permit = Arc::new(AtomicU8::new(CANCELLABLE));
        self.tx
            .try_send(Job {
                id,
                edit,
                permit: permit.clone(),
            })
            .map_err(|_| io::ErrorKind::WouldBlock)?;
        state.jobs.insert(
            id,
            Slot {
                status: status.clone(),
                permit,
            },
        );
        while state.jobs.len() > 16 {
            state.jobs.pop_first();
        }
        Ok(status)
    }
    pub(super) fn status(&self, id: u64) -> io::Result<Vec<Status>> {
        let state = self.state.lock().map_err(|_| io::ErrorKind::Other)?;
        if id == 0 {
            Ok(state.jobs.values().map(|s| s.status.clone()).collect())
        } else {
            Ok(vec![
                state
                    .jobs
                    .get(&id)
                    .ok_or(io::ErrorKind::NotFound)?
                    .status
                    .clone(),
            ])
        }
    }
    pub(super) fn cancel(&self, id: u64) -> io::Result<Status> {
        let mut state = self.state.lock().map_err(|_| io::ErrorKind::Other)?;
        let slot = state.jobs.get_mut(&id).ok_or(io::ErrorKind::NotFound)?;
        if !slot.status.state.finished() {
            if let Err(previous) = slot.permit.compare_exchange(
                CANCELLABLE,
                CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) && previous != CANCELLED
            {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            // Keep it unfinished until its staged file and lock have been released.
            slot.status.reason = Some("cancellation_requested");
        }
        Ok(slot.status.clone())
    }
}

pub(super) enum Admission {
    Busy,
    Cancelled,
    Published(Result<Reload, ()>),
    SaveFailed,
}
type Publication = (Prepared, Admission);
pub(super) enum Command {
    Publish {
        prepared: Prepared,
        permit: Arc<AtomicU8>,
        generation: u64,
        reply: SyncSender<Publication>,
    },
    Reload {
        generation: u64,
        reply: SyncSender<Result<Reload, ()>>,
    },
    Status(SyncSender<Result<Reload, ()>>),
}
impl Command {
    /// Returns true only when the transport failed and requires conservative recovery.
    pub(super) fn execute(self, engine: &mut Engine<Mozc>) -> bool {
        match self {
            Self::Publish {
                mut prepared,
                permit,
                generation,
                reply,
            } => {
                let mut failed = false;
                let admission = if permit.load(Ordering::Acquire) == CANCELLED {
                    Admission::Cancelled
                } else if engine.last_activity.elapsed() < Duration::from_secs(2)
                    || engine
                        .sessions
                        .values()
                        .any(|s| s.pending.is_some() || !s.snapshot.preedit.segments.is_empty())
                {
                    Admission::Busy
                } else {
                    // Do not replace a file still being read by an earlier loader.
                    match engine.backend.dictionary_reload_status() {
                        Ok(status)
                            if matches!(status.state, ReloadState::Loading | ReloadState::Busy) =>
                        {
                            Admission::Busy
                        }
                        Err(_) => {
                            failed = true;
                            Admission::Busy
                        }
                        Ok(_) => {
                            if permit
                                .compare_exchange(
                                    CANCELLABLE,
                                    PUBLISHING,
                                    Ordering::AcqRel,
                                    Ordering::Acquire,
                                )
                                .is_err()
                            {
                                Admission::Cancelled
                            } else if prepared.publish().is_err() {
                                Admission::SaveFailed
                            } else {
                                let result =
                                    engine.backend.reload_dictionary(generation).map_err(|_| ());
                                failed = result.is_err();
                                Admission::Published(result)
                            }
                        }
                    }
                };
                let _ = reply.send((prepared, admission));
                failed
            }
            Self::Reload { generation, reply } => {
                let result = engine.backend.reload_dictionary(generation).map_err(|_| ());
                let failed = result.is_err();
                let _ = reply.send(result);
                failed
            }
            Self::Status(reply) => {
                let result = engine.backend.dictionary_reload_status().map_err(|_| ());
                let failed = result.is_err();
                let _ = reply.send(result);
                failed
            }
        }
    }
}

fn update(
    state: &Mutex<State>,
    id: u64,
    stage: Stage,
    revision: Option<String>,
    reason: Option<&'static str>,
) {
    if let Ok(mut state) = state.lock()
        && let Some(slot) = state.jobs.get_mut(&id)
    {
        slot.status.state = stage;
        if revision.is_some() {
            slot.status.saved_revision = revision;
        }
        slot.status.reason = reason;
    }
}
fn run(
    job: Job,
    profile: &Path,
    engine: &SyncSender<Work>,
    state: &Mutex<State>,
    generation: &mut u64,
) {
    let Job { id, edit, permit } = job;
    update(state, id, Stage::Preparing, None, None);
    let mut prepared = match storage::prepare(profile, edit) {
        Ok(prepared) => prepared,
        Err(error) => {
            let reason = match error {
                storage::Error::Conflict => "revision_conflict",
                storage::Error::Unsupported => "unsupported_storage",
                storage::Error::Invalid => "invalid_edit",
                storage::Error::Busy => "writer_busy",
                storage::Error::Io(_) => "storage_unavailable",
            };
            update(state, id, Stage::Failed, None, Some(reason));
            return;
        }
    };
    let Some(first_generation) = generation.checked_add(1) else {
        drop(prepared);
        update(state, id, Stage::Failed, None, Some("generation_exhausted"));
        return;
    };
    *generation = first_generation;
    update(state, id, Stage::WaitingForIdle, None, None);
    let deadline = Instant::now() + Duration::from_secs(600);
    let initial = loop {
        if permit.load(Ordering::Acquire) == CANCELLED || Instant::now() > deadline {
            // Prevent a queued publication from winning cancellation after timeout.
            permit.store(CANCELLED, Ordering::Release);
            drop(prepared);
            update(state, id, Stage::Cancelled, None, None);
            return;
        }
        let (reply, done) = mpsc::sync_channel(1);
        let work = Work::Dictionary(Box::new(Command::Publish {
            prepared,
            permit: permit.clone(),
            generation: *generation,
            reply,
        }));
        match engine.try_send(work) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(Work::Dictionary(command))) => {
                let Command::Publish {
                    prepared: returned, ..
                } = *command
                else {
                    unreachable!()
                };
                prepared = returned;
                std::thread::sleep(POLL);
                continue;
            }
            Err(_) => {
                update(state, id, Stage::Failed, None, Some("engine_unavailable"));
                return;
            }
        }
        let Ok((returned, admission)) = done.recv() else {
            update(state, id, Stage::Failed, None, Some("engine_unavailable"));
            return;
        };
        prepared = returned;
        match admission {
            Admission::Busy => std::thread::sleep(POLL),
            Admission::Cancelled => {
                drop(prepared);
                update(state, id, Stage::Cancelled, None, None);
                return;
            }
            Admission::SaveFailed => {
                drop(prepared);
                update(state, id, Stage::Failed, None, Some("save_failed"));
                return;
            }
            Admission::Published(result) => break result,
        }
    };
    let durable = prepared.sync_directory().is_ok();
    update(
        state,
        id,
        Stage::Loading,
        Some(prepared.revision().to_owned()),
        None,
    );
    let loaded = load(engine, generation, initial);
    // Keep the writer lock until the load completes. Cleanup happens on this thread.
    drop(prepared);
    let reason = if !loaded {
        Some("reload_failed")
    } else if !durable {
        Some("directory_sync_failed")
    } else {
        None
    };
    update(
        state,
        id,
        if reason.is_none() {
            Stage::Ready
        } else {
            Stage::Failed
        },
        None,
        reason,
    );
}

fn query(engine: &SyncSender<Work>, generation: Option<u64>) -> Result<Reload, ()> {
    let (reply, done) = mpsc::sync_channel(1);
    let command = match generation {
        Some(generation) => Command::Reload { generation, reply },
        None => Command::Status(reply),
    };
    let mut work = Work::Dictionary(Box::new(command));
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match engine.try_send(work) {
            Ok(()) => break,
            Err(mpsc::TrySendError::Full(returned)) if Instant::now() < deadline => {
                work = returned;
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return Err(()),
        }
    }
    done.recv().map_err(|_| ())?
}
fn load(engine: &SyncSender<Work>, generation: &mut u64, initial: Result<Reload, ()>) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut current = initial;
    let mut attempts = 1;
    while Instant::now() < deadline {
        match current {
            Ok(status)
                if status.generation == *generation && status.state == ReloadState::Ready =>
            {
                return true;
            }
            Ok(status)
                if status.generation == *generation && status.state == ReloadState::Loading =>
            {
                std::thread::sleep(POLL);
                current = query(engine, None);
            }
            Ok(status)
                if status.state == ReloadState::Loading || status.state == ReloadState::Busy =>
            {
                std::thread::sleep(POLL);
                current = query(engine, None);
            }
            _ => {
                if attempts >= 3 {
                    return false;
                }
                let Some(next) = generation.checked_add(1) else {
                    return false;
                };
                *generation = next;
                attempts += 1;
                std::thread::sleep(POLL);
                current = query(engine, Some(*generation));
            }
        }
    }
    false
}
