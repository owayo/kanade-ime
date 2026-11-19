// SPDX-License-Identifier: MIT
//! Bounded background service between the engine's key loop and the worker.
//!
//! The key loop only copies a small job into a short queue and later looks a
//! result up. Every wait, spawn, timeout and restart happens on this thread.
use crate::{
    Job,
    frame::Fault,
    limits,
    process::{Failure, Worker},
    profile::Model,
    store::Reason,
};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard},
    time::{Duration, Instant},
};

/// What the key loop sees when it asks for a job's result.
#[derive(Clone, Debug, PartialEq)]
pub enum Poll {
    Pending,
    /// One score per submitted candidate; `None` means "leave it where it is".
    Ready(Vec<Option<f32>>),
    /// Dropped, cancelled, failed or expired. It will never arrive.
    Gone,
}

/// The engine-facing interface. Every method is non-blocking in practice: it
/// takes a lock that is only ever held for a few memory operations.
pub trait Rerank: Send {
    /// Queues a job. `None` when the service is inactive or the job is refused.
    fn submit(&self, job: Job) -> Option<u64>;
    fn submit_sentence(&self, job: Job) -> Option<u64> {
        self.submit(job)
    }
    fn take(&self, id: u64) -> Poll;
    /// Drops a queued job or discards the result of a running one.
    fn cancel(&self, id: u64);
}

pub trait Backend: Send {
    fn score(&mut self, job: &Job, sentence: bool) -> Result<Vec<Option<f32>>, Failure>;
}
pub enum StartError {
    /// The process could not be started or loaded; worth another attempt.
    Failed,
    /// The model or runtime is not the qualified one. Never retried.
    Rejected(Reason),
}
pub trait Spawner: Send + 'static {
    /// Starts a backend that serves the file at `path` as the registered
    /// `model`, under that model's profile and no other.
    fn start(&self, path: &Path, model: &'static Model) -> Result<Box<dyn Backend>, StartError>;
}

impl Backend for Worker {
    fn score(&mut self, job: &Job, sentence: bool) -> Result<Vec<Option<f32>>, Failure> {
        Worker::score_conversion(
            self,
            job,
            limits::BUDGET_MS,
            Duration::from_millis(limits::HARD_TIMEOUT_MS),
            sentence,
        )
    }
}
/// Starts the real worker and refuses it unless it loaded the selected model
/// with that model's profile.
pub struct ProcessSpawner {
    pub worker: PathBuf,
}
impl Spawner for ProcessSpawner {
    fn start(&self, path: &Path, model: &'static Model) -> Result<Box<dyn Backend>, StartError> {
        let mut worker = Worker::spawn(&self.worker).map_err(|failure| match failure {
            Failure::Runtime => StartError::Rejected(Reason::RuntimeChanged),
            _ => StartError::Failed,
        })?;
        worker
            .load(path, model, limits::THREADS)
            .map_err(|failure| match failure {
                // Another file, vocabulary or profile than the registered one.
                Failure::Identity
                | Failure::Refused(
                    Fault::ProfileMismatch | Fault::UnknownModel | Fault::ModelChanged,
                ) => StartError::Rejected(Reason::ModelChanged),
                _ => StartError::Failed,
            })?;
        Ok(Box::new(worker))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    Inactive,
    Starting,
    Running,
    /// Waiting before another start attempt.
    Backoff,
    /// Stopped after repeated failures or an identity mismatch.
    Faulted(Reason),
}

#[derive(Clone, Copy)]
pub struct Tuning {
    /// Waits before the second and third start attempts.
    pub backoff: [Duration; 2],
    /// An unused worker is stopped after this long.
    pub idle: Duration,
    pub result_ttl: Duration,
}
impl Default for Tuning {
    fn default() -> Self {
        Self {
            backoff: [Duration::from_secs(1), Duration::from_secs(10)],
            idle: Duration::from_secs(600),
            result_ttl: Duration::from_millis(limits::RESULT_TTL_MS),
        }
    }
}
const MAX_FAILURES: u32 = 3;
const MAX_RESULTS: usize = 8;

struct Queued {
    sentence: bool,
    id: u64,
    job: Job,
    submitted: Instant,
}
/// What is being served: a file and the registered model it was imported as.
#[derive(Clone)]
struct Target {
    path: PathBuf,
    model: &'static Model,
}
struct State {
    target: Option<Target>,
    generation: u64,
    queue: VecDeque<Queued>,
    active: Option<u64>,
    discard_active: bool,
    results: VecDeque<(u64, Vec<Option<f32>>)>,
    next: u64,
    health: Health,
    spawner: Option<Box<dyn Spawner>>,
    running: bool,
    stop: bool,
    /// Scoring requests actually sent to a backend; for tests and status.
    scored: u64,
}
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    tuning: Tuning,
}
impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock leaves plain data; keep serving.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Clone)]
pub struct Service(Arc<Shared>);
impl Service {
    pub fn new(spawner: impl Spawner) -> Self {
        Self::with(spawner, Tuning::default())
    }
    pub fn with(spawner: impl Spawner, tuning: Tuning) -> Self {
        Self(Arc::new(Shared {
            state: Mutex::new(State {
                target: None,
                generation: 0,
                queue: VecDeque::new(),
                active: None,
                discard_active: false,
                results: VecDeque::new(),
                next: 0,
                health: Health::Inactive,
                spawner: Some(Box::new(spawner)),
                running: false,
                stop: false,
                scored: 0,
            }),
            wake: Condvar::new(),
            tuning,
        }))
    }
    /// Starts serving the file at `path` as the registered `model`. Queued
    /// jobs, results and cached scores of any earlier model, profile or
    /// configuration are dropped.
    pub fn activate(&self, path: PathBuf, model: &'static Model) {
        let mut state = self.0.lock();
        reset(&mut state);
        state.target = Some(Target { path, model });
        state.health = Health::Starting;
        if !state.running
            && let Some(spawner) = state.spawner.take()
        {
            let shared = self.0.clone();
            state.running = std::thread::Builder::new()
                .name("kanade-llm".into())
                .spawn(move || run(shared, spawner))
                .is_ok();
            if !state.running {
                state.health = Health::Faulted(Reason::WorkerFailed);
            }
        }
        drop(state);
        self.0.wake.notify_all();
    }
    /// Stops the worker and forgets every job, result and cached score.
    pub fn deactivate(&self) {
        let mut state = self.0.lock();
        reset(&mut state);
        state.target = None;
        state.health = Health::Inactive;
        drop(state);
        self.0.wake.notify_all();
    }
    pub fn health(&self) -> Health {
        self.0.lock().health
    }
    pub fn active(&self) -> bool {
        self.0.lock().target.is_some()
    }
    /// ID of the registered model being served, if any.
    pub fn serving(&self) -> Option<&'static str> {
        self.0.lock().target.as_ref().map(|target| target.model.id)
    }
    /// Number of scoring requests that reached a backend.
    pub fn scored(&self) -> u64 {
        self.0.lock().scored
    }
}
fn reset(state: &mut State) {
    state.generation += 1;
    state.queue.clear();
    state.results.clear();
    state.discard_active = true;
}
impl Service {
    fn enqueue(&self, job: Job, sentence: bool) -> Option<u64> {
        if !job.valid() {
            return None;
        }
        // The profile check runs outside the lock; the generation tells
        // whether the model changed meanwhile.
        let (model, generation) = {
            let state = self.0.lock();
            match (&state.target, state.health) {
                (Some(_), Health::Faulted(_)) | (None, _) => return None,
                (Some(target), _) => (target.model, state.generation),
            }
        };
        // Text with a marker of the active profile is never queued or scored.
        if !model.profile.prompt.admits(&job) {
            return None;
        }
        let mut state = self.0.lock();
        if state.generation != generation
            || state.target.is_none()
            || matches!(state.health, Health::Faulted(_))
        {
            return None;
        }
        state.next += 1;
        let id = state.next;
        // The newest request is the one still on screen; drop the oldest.
        while state.queue.len() >= limits::QUEUE {
            state.queue.pop_front();
        }
        state.queue.push_back(Queued {
            sentence,
            id,
            job,
            submitted: Instant::now(),
        });
        drop(state);
        self.0.wake.notify_all();
        Some(id)
    }
}
impl Rerank for Service {
    fn submit(&self, job: Job) -> Option<u64> {
        self.enqueue(job, false)
    }
    fn submit_sentence(&self, job: Job) -> Option<u64> {
        self.enqueue(job, true)
    }
    fn take(&self, id: u64) -> Poll {
        let mut state = self.0.lock();
        if let Some(index) = state.results.iter().position(|(done, _)| *done == id) {
            return state
                .results
                .remove(index)
                .map_or(Poll::Gone, |(_, scores)| Poll::Ready(scores));
        }
        if state.queue.iter().any(|queued| queued.id == id)
            || (state.active == Some(id) && !state.discard_active)
        {
            Poll::Pending
        } else {
            Poll::Gone
        }
    }
    fn cancel(&self, id: u64) {
        let mut state = self.0.lock();
        state.queue.retain(|queued| queued.id != id);
        state.results.retain(|(done, _)| *done != id);
        if state.active == Some(id) {
            state.discard_active = true;
        }
    }
}

enum Action {
    Reset,
    Start(Target),
    Score(Queued),
    Sleep,
    Stop,
}

fn run(shared: Arc<Shared>, spawner: Box<dyn Spawner>) {
    let tuning = shared.tuning;
    let mut backend: Option<Box<dyn Backend>> = None;
    let mut generation = 0;
    let mut failures = 0u32;
    let mut retry_at: Option<Instant> = None;
    let mut asleep = false;
    let mut last_use = Instant::now();
    let mut cache: VecDeque<([u8; 32], Vec<Option<f32>>)> = VecDeque::new();
    // Identity of the model and profile the running backend was started for.
    let mut identity = [0u8; 32];
    loop {
        let action = {
            let mut state = shared.lock();
            loop {
                if state.stop {
                    break Action::Stop;
                }
                if state.generation != generation {
                    generation = state.generation;
                    break Action::Reset;
                }
                let now = Instant::now();
                let target = state.target.clone();
                let wait = match (target, backend.is_some()) {
                    (None, _) => None,
                    (Some(_), false) if matches!(state.health, Health::Faulted(_)) => None,
                    (Some(_), false) if asleep && state.queue.is_empty() => None,
                    (Some(target), false) => match retry_at {
                        Some(at) if now < at => Some(at - now),
                        _ => {
                            state.health = Health::Starting;
                            break Action::Start(target);
                        }
                    },
                    (Some(_), true) => {
                        if let Some(queued) = state.queue.pop_front() {
                            state.active = Some(queued.id);
                            state.discard_active = false;
                            break Action::Score(queued);
                        }
                        let idle = now.saturating_duration_since(last_use);
                        if idle >= tuning.idle {
                            break Action::Sleep;
                        }
                        Some(tuning.idle - idle)
                    }
                };
                state = match wait {
                    Some(wait) => {
                        shared
                            .wake
                            .wait_timeout(state, wait)
                            .unwrap_or_else(|e| e.into_inner())
                            .0
                    }
                    None => shared.wake.wait(state).unwrap_or_else(|e| e.into_inner()),
                };
            }
        };
        match action {
            Action::Stop => return,
            Action::Reset => {
                backend = None;
                cache.clear();
                failures = 0;
                retry_at = None;
                asleep = false;
            }
            Action::Sleep => {
                backend = None;
                asleep = true;
            }
            Action::Start(target) => {
                asleep = false;
                identity = served(target.model);
                let started = spawner.start(&target.path, target.model);
                let mut state = shared.lock();
                let current = state.generation == generation;
                match started {
                    Ok(started) => {
                        failures = 0;
                        retry_at = None;
                        last_use = Instant::now();
                        if current {
                            backend = Some(started);
                            state.health = Health::Running;
                        }
                    }
                    Err(StartError::Rejected(reason)) => {
                        if current {
                            fault(&mut state, reason);
                        }
                    }
                    Err(StartError::Failed) => {
                        failures += 1;
                        if failures >= MAX_FAILURES {
                            if current {
                                fault(&mut state, Reason::WorkerFailed);
                            }
                        } else {
                            retry_at = Some(Instant::now() + tuning.backoff[failures as usize - 1]);
                            if current {
                                state.health = Health::Backoff;
                            }
                        }
                    }
                }
            }
            Action::Score(queued) => {
                let mut outcome = None;
                // A request older than the display deadline is not worth scoring.
                if queued.submitted.elapsed() <= tuning.result_ttl {
                    let key = digest(&identity, &queued.job, queued.sentence);
                    if let Some((_, scores)) = cache.iter().find(|(cached, _)| *cached == key) {
                        outcome = Some(scores.clone());
                    } else if let Some(worker) = backend.as_mut() {
                        shared.lock().scored += 1;
                        match worker.score(&queued.job, queued.sentence) {
                            Ok(scores) if scores.len() == queued.job.candidates.len() => {
                                failures = 0;
                                cache.push_back((key, scores.clone()));
                                while cache.len() > limits::CACHE {
                                    cache.pop_front();
                                }
                                outcome = Some(scores);
                            }
                            Err(failure) if failure.recoverable() => {}
                            _ => {
                                // Timeout, crash or malformed output: the process
                                // is already killed. Restart with a delay.
                                backend = None;
                                failures += 1;
                                if failures < MAX_FAILURES {
                                    retry_at = Some(
                                        Instant::now() + tuning.backoff[failures as usize - 1],
                                    );
                                }
                            }
                        }
                    }
                    last_use = Instant::now();
                }
                let mut state = shared.lock();
                if state.generation == generation {
                    if let Some(scores) = outcome
                        && state.active == Some(queued.id)
                        && !state.discard_active
                    {
                        state.results.push_back((queued.id, scores));
                        while state.results.len() > MAX_RESULTS {
                            state.results.pop_front();
                        }
                    }
                    if backend.is_none() {
                        if failures >= MAX_FAILURES {
                            fault(&mut state, Reason::WorkerFailed);
                        } else {
                            state.health = Health::Backoff;
                        }
                    }
                }
                state.active = None;
            }
        }
    }
}
fn fault(state: &mut State, reason: Reason) {
    state.health = Health::Faulted(reason);
    state.queue.clear();
    state.results.clear();
}
/// Identity of everything that determines a score besides the job: the file
/// and the whole profile.
fn served(model: &Model) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(model.id.as_bytes());
    digest.update([0]);
    digest.update(model.sha256.as_bytes());
    digest.update(model.profile.digest());
    digest.finalize().into()
}
/// Cache key. A digest, so the cache never holds input text. It includes the
/// served identity: a score is never reused for another model or profile.
fn digest(identity: &[u8; 32], job: &Job, sentence: bool) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(identity);
    digest.update([u8::from(sentence)]);
    for field in [&job.context, &job.reading, &job.prefix, &job.suffix]
        .into_iter()
        .chain(&job.candidates)
    {
        digest.update((field.len() as u32).to_le_bytes());
        digest.update(field.as_bytes());
    }
    digest.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{CATALOG, fixture};
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tsumugi() -> &'static Model {
        &CATALOG[0]
    }

    fn job(tag: &str) -> Job {
        Job {
            context: format!("文脈{tag}"),
            reading: "コウエン".into(),
            prefix: String::new(),
            suffix: String::new(),
            candidates: vec!["公園".into(), "講演".into()],
        }
    }
    #[derive(Default)]
    struct ScoreGate {
        // Number of calls entered, then permits available to finish them.
        state: Mutex<(usize, usize)>,
        changed: Condvar,
    }
    impl ScoreGate {
        fn enter(&self) -> Result<(), Failure> {
            let mut state = self.state.lock().unwrap();
            state.0 += 1;
            self.changed.notify_all();
            let (mut state, _) = self
                .changed
                .wait_timeout_while(state, Duration::from_secs(10), |state| state.1 == 0)
                .unwrap();
            if state.1 == 0 {
                return Err(Failure::Timeout);
            }
            state.1 -= 1;
            Ok(())
        }
        fn wait_entered(&self, count: usize) {
            let (state, _) = self
                .changed
                .wait_timeout_while(
                    self.state.lock().unwrap(),
                    Duration::from_secs(5),
                    |state| state.0 < count,
                )
                .unwrap();
            assert!(state.0 >= count, "backend did not enter scoring");
        }
        fn release(&self, count: usize) {
            self.state.lock().unwrap().1 += count;
            self.changed.notify_all();
        }
    }
    #[derive(Clone, Default)]
    struct Script {
        starts: Arc<AtomicU32>,
        stops: Arc<AtomicU32>,
        /// Start attempts that fail before one succeeds.
        failing_starts: u32,
        reject: bool,
        delay: Duration,
        gate: Option<Arc<ScoreGate>>,
        /// Scoring calls answered with a fatal failure.
        crashes: Arc<AtomicU32>,
        /// Model IDs the service asked a backend for, in order.
        models: Arc<Mutex<Vec<&'static str>>>,
    }
    struct Fake(Script, &'static Model);
    impl Drop for Fake {
        fn drop(&mut self) {
            self.0.stops.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Backend for Fake {
        fn score(&mut self, job: &Job, _sentence: bool) -> Result<Vec<Option<f32>>, Failure> {
            if let Some(gate) = &self.0.gate {
                gate.enter()?;
            }
            std::thread::sleep(self.0.delay);
            if self
                .0
                .crashes
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                return Err(Failure::Timeout);
            }
            // Scores depend on the profile, as real ones do.
            let base = self.1.profile.end as f32;
            Ok((0..job.candidates.len())
                .map(|i| Some(-(i as f32) - base + 2.0))
                .collect())
        }
    }
    impl Spawner for Script {
        fn start(&self, _: &Path, model: &'static Model) -> Result<Box<dyn Backend>, StartError> {
            let attempt = self.starts.fetch_add(1, Ordering::SeqCst);
            self.models.lock().unwrap().push(model.id);
            if self.reject {
                return Err(StartError::Rejected(Reason::ModelChanged));
            }
            if attempt < self.failing_starts {
                return Err(StartError::Failed);
            }
            Ok(Box::new(Fake(self.clone(), model)))
        }
    }
    fn tuning() -> Tuning {
        Tuning {
            backoff: [Duration::from_millis(20), Duration::from_millis(40)],
            idle: Duration::from_secs(60),
            result_ttl: Duration::from_millis(limits::RESULT_TTL_MS),
        }
    }
    fn wait(service: &Service, id: u64) -> Poll {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service.take(id) {
                Poll::Pending if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                other => return other,
            }
        }
    }
    fn until(service: &Service, health: impl Fn(Health) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !health(service.health()) {
            assert!(Instant::now() < deadline, "health {:?}", service.health());
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn inactive_service_accepts_nothing_and_starts_no_worker() {
        let script = Script::default();
        let service = Service::with(script.clone(), tuning());
        assert_eq!(service.submit(job("a")), None);
        assert_eq!(service.take(1), Poll::Gone);
        assert_eq!(script.starts.load(Ordering::SeqCst), 0);
        assert_eq!(service.health(), Health::Inactive);
    }
    #[test]
    fn scores_arrive_and_identical_requests_are_not_recomputed() {
        let script = Script::default();
        let service = Service::with(script.clone(), tuning());
        service.activate("model".into(), tsumugi());
        let first = service.submit(job("a")).unwrap();
        assert_eq!(
            wait(&service, first),
            Poll::Ready(vec![Some(-257.0), Some(-258.0)])
        );
        // Taken once; a second read cannot observe it again.
        assert_eq!(service.take(first), Poll::Gone);
        let again = service.submit(job("a")).unwrap();
        assert_eq!(
            wait(&service, again),
            Poll::Ready(vec![Some(-257.0), Some(-258.0)])
        );
        assert_eq!(service.scored(), 1);
        let other = service.submit(job("b")).unwrap();
        assert!(matches!(wait(&service, other), Poll::Ready(_)));
        assert_eq!(service.scored(), 2);
        assert_eq!(script.starts.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn malformed_jobs_are_refused_before_queueing() {
        let service = Service::with(Script::default(), tuning());
        service.activate("model".into(), tsumugi());
        let mut bad = job("a");
        bad.candidates.truncate(1);
        assert_eq!(service.submit(bad), None);
        let mut bad = job("a");
        bad.context.clear();
        assert_eq!(service.submit(bad), None);
        assert_eq!(service.scored(), 0);
    }
    #[test]
    fn queue_overflow_drops_the_oldest_request() {
        let gate = Arc::new(ScoreGate::default());
        let script = Script {
            gate: Some(gate.clone()),
            ..Default::default()
        };
        let service = Service::with(
            script,
            Tuning {
                result_ttl: Duration::from_secs(30),
                ..tuning()
            },
        );
        service.activate("model".into(), tsumugi());
        until(&service, |h| h == Health::Running);
        let running = service.submit(job("0")).unwrap();
        gate.wait_entered(1);
        let ids: Vec<u64> = (1..=limits::QUEUE + 2)
            .map(|i| service.submit(job(&i.to_string())).unwrap())
            .collect();
        // Only the newest `QUEUE` requests are still waiting.
        let (dropped, kept) = ids.split_at(ids.len() - limits::QUEUE);
        assert!(dropped.iter().all(|id| service.take(*id) == Poll::Gone));
        assert!(kept.iter().all(|id| service.take(*id) == Poll::Pending));
        gate.release(1 + limits::QUEUE);
        assert!(matches!(wait(&service, running), Poll::Ready(_)));
        assert!(
            kept.iter()
                .all(|id| matches!(wait(&service, *id), Poll::Ready(_)))
        );
        assert_eq!(service.scored(), 1 + limits::QUEUE as u64);
    }
    #[test]
    fn cancelled_and_expired_requests_never_produce_a_result() {
        let gate = Arc::new(ScoreGate::default());
        let script = Script {
            gate: Some(gate.clone()),
            ..Default::default()
        };
        let service = Service::with(
            script,
            Tuning {
                result_ttl: Duration::from_secs(30),
                ..tuning()
            },
        );
        service.activate("model".into(), tsumugi());
        until(&service, |h| h == Health::Running);
        let running = service.submit(job("a")).unwrap();
        gate.wait_entered(1);
        // Keep the first score blocked while making the queued request stale.
        let expired = service.submit(job("b")).unwrap();
        service.0.lock().queue.front_mut().unwrap().submitted =
            Instant::now() - Duration::from_secs(60);
        service.cancel(running);
        assert_eq!(service.take(running), Poll::Gone);
        gate.release(1);
        assert_eq!(wait(&service, expired), Poll::Gone);
        assert_eq!(service.take(running), Poll::Gone);
        assert_eq!(service.scored(), 1);
    }
    #[test]
    fn deactivation_and_model_change_drop_everything() {
        let gate = Arc::new(ScoreGate::default());
        let script = Script {
            gate: Some(gate.clone()),
            ..Default::default()
        };
        let service = Service::with(
            script.clone(),
            Tuning {
                result_ttl: Duration::from_secs(30),
                ..tuning()
            },
        );
        service.activate("model".into(), tsumugi());
        until(&service, |h| h == Health::Running);
        let running = service.submit(job("a")).unwrap();
        gate.wait_entered(1);
        let queued = service.submit(job("b")).unwrap();
        service.deactivate();
        assert_eq!(service.take(running), Poll::Gone);
        assert_eq!(service.take(queued), Poll::Gone);
        assert_eq!(service.submit(job("c")), None);
        gate.release(1);
        let deadline = Instant::now() + Duration::from_secs(5);
        while script.stops.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "old backend was not stopped");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(service.take(running), Poll::Gone);
        // A new model starts a new worker and an empty cache.
        service.activate("other".into(), tsumugi());
        gate.release(1);
        let fresh = service.submit(job("a")).unwrap();
        assert!(matches!(wait(&service, fresh), Poll::Ready(_)));
        assert_eq!(script.starts.load(Ordering::SeqCst), 2);
        assert_eq!(service.scored(), 2);
    }
    #[test]
    fn a_timed_out_worker_is_restarted_after_a_delay() {
        let script = Script {
            crashes: Arc::new(AtomicU32::new(1)),
            ..Default::default()
        };
        let service = Service::with(script.clone(), tuning());
        service.activate("model".into(), tsumugi());
        let lost = service.submit(job("a")).unwrap();
        assert_eq!(wait(&service, lost), Poll::Gone);
        until(&service, |h| h == Health::Running);
        assert_eq!(script.starts.load(Ordering::SeqCst), 2);
        let next = service.submit(job("a")).unwrap();
        assert!(matches!(wait(&service, next), Poll::Ready(_)));
    }
    #[test]
    fn repeated_failures_stop_the_service_until_reactivation() {
        let script = Script {
            failing_starts: 100,
            ..Default::default()
        };
        let service = Service::with(script.clone(), tuning());
        service.activate("model".into(), tsumugi());
        until(&service, |h| h == Health::Faulted(Reason::WorkerFailed));
        assert_eq!(script.starts.load(Ordering::SeqCst), MAX_FAILURES);
        assert_eq!(service.submit(job("a")), None);
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(script.starts.load(Ordering::SeqCst), MAX_FAILURES);
        service.activate("model".into(), tsumugi());
        until(&service, |h| h == Health::Faulted(Reason::WorkerFailed));
        assert_eq!(script.starts.load(Ordering::SeqCst), MAX_FAILURES * 2);
    }
    #[test]
    fn an_unqualified_model_is_never_retried() {
        let script = Script {
            reject: true,
            ..Default::default()
        };
        let service = Service::with(script.clone(), tuning());
        service.activate("model".into(), tsumugi());
        until(&service, |h| h == Health::Faulted(Reason::ModelChanged));
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(script.starts.load(Ordering::SeqCst), 1);
        assert_eq!(service.submit(job("a")), None);
    }
    #[test]
    fn an_idle_worker_is_stopped_and_started_again_on_demand() {
        let script = Script::default();
        let service = Service::with(
            script.clone(),
            Tuning {
                idle: Duration::from_millis(30),
                ..tuning()
            },
        );
        service.activate("model".into(), tsumugi());
        let first = service.submit(job("a")).unwrap();
        assert!(matches!(wait(&service, first), Poll::Ready(_)));
        // Elapsed time alone does not prove the service thread ran its idle
        // cleanup. Wait for the backend to be dropped before submitting work.
        let deadline = Instant::now() + Duration::from_secs(5);
        while script.stops.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "idle worker was not stopped");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(script.stops.load(Ordering::SeqCst), 1);
        assert_eq!(script.starts.load(Ordering::SeqCst), 1);
        let second = service.submit(job("b")).unwrap();
        assert!(matches!(wait(&service, second), Poll::Ready(_)));
        assert_eq!(script.starts.load(Ordering::SeqCst), 2);
    }
    #[test]
    fn the_selected_model_reaches_the_backend_and_scores_are_not_shared() {
        let script = Script::default();
        let service = Service::with(script.clone(), tuning());
        assert_eq!(service.serving(), None);
        service.activate("model".into(), tsumugi());
        assert_eq!(service.serving(), Some(tsumugi().id));
        let first = service.submit(job("a")).unwrap();
        assert_eq!(
            wait(&service, first),
            Poll::Ready(vec![Some(-257.0), Some(-258.0)])
        );
        assert_eq!(service.scored(), 1);
        // The same file under another profile: the same job is scored again
        // by a new backend, with that profile's result.
        let alt = fixture::alt();
        service.activate("model".into(), alt);
        assert_eq!(service.serving(), Some(alt.id));
        let second = service.submit(job("a")).unwrap();
        assert_eq!(
            wait(&service, second),
            Poll::Ready(vec![Some(0.0), Some(-1.0)])
        );
        assert_eq!(service.scored(), 2);
        assert_eq!(*script.models.lock().unwrap(), [tsumugi().id, alt.id]);
        service.deactivate();
        assert_eq!(service.serving(), None);
        // Keys of one request differ between models and between profiles.
        let keys = [tsumugi(), alt, fixture::other()]
            .map(|model| digest(&served(model), &job("a"), false));
        assert_ne!(
            digest(&served(tsumugi()), &job("a"), false),
            digest(&served(tsumugi()), &job("a"), true)
        );
        assert!(keys[0] != keys[1] && keys[1] != keys[2] && keys[0] != keys[2]);
    }
    #[test]
    fn a_reserved_marker_of_the_active_profile_is_refused_before_queueing() {
        let script = Script::default();
        let service = Service::with(script.clone(), tuning());
        let marked = |marker: &str| {
            let mut job = job("a");
            job.candidates[1].push_str(marker);
            job
        };
        service.activate("model".into(), tsumugi());
        until(&service, |h| h == Health::Running);
        for field in 0..5 {
            let mut bad = job("a");
            let text = match field {
                0 => &mut bad.context,
                1 => &mut bad.reading,
                2 => &mut bad.prefix,
                3 => &mut bad.suffix,
                _ => &mut bad.candidates[0],
            };
            text.push('\u{E082}');
            assert_eq!(service.submit(bad), None);
        }
        assert_eq!(service.submit(marked("\u{E08F}")), None);
        assert_eq!(service.scored(), 0);
        // Another profile's marker is ordinary text for this one.
        let id = service.submit(marked("「")).unwrap();
        assert!(matches!(wait(&service, id), Poll::Ready(_)));
        assert_eq!(service.scored(), 1);
        // And the other way round once that profile is the active one.
        service.activate("model".into(), fixture::alt());
        until(&service, |h| h == Health::Running);
        assert_eq!(service.submit(marked("「")), None);
        let id = service.submit(marked("\u{E082}")).unwrap();
        assert!(matches!(wait(&service, id), Poll::Ready(_)));
        assert_eq!(service.scored(), 2);
    }
}
