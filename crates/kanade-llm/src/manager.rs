// SPDX-License-Identifier: MIT
//! Model management for the engine's authenticated administration path.
//!
//! Imports, hashing and qualification run on one management thread. Status
//! reads are cheap and never touch a file.
use crate::{
    profile::{self, Model},
    qualify::{self, Measurement, Phase, Refusal},
    service::{Health, ProcessSpawner, Service},
    store::{self, Evidence, ImportError, Install, Reason, Recorded, Store},
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        mpsc::{self, SyncSender},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    /// Records have not been read yet; nothing is served.
    Loading,
    NotInstalled,
    /// A model is present but has no valid qualification; see `reason`.
    Installed,
    Qualifying,
    Qualified,
    Faulted,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Import,
    Qualify,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobPhase {
    Queued,
    Copying,
    StartingRuntime,
    Tokenizer,
    Bounds,
    Ranking,
    Activating,
    Done,
    Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobError {
    None,
    SourceUnreadable,
    ModelMismatch,
    Storage,
    NotInstalled,
    WorkerMissing,
    WorkerFailed,
    IdentityMismatch,
    TokenizerMismatch,
    QualityGate,
    /// The stored consent does not cover the current terms; import again.
    ConsentRequired,
    ActivationFailed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct JobStatus {
    pub id: u64,
    pub kind: JobKind,
    pub phase: JobPhase,
    /// Percent of the model copied; 100 once copying is over.
    pub copied: u8,
    pub error: JobError,
}
impl JobStatus {
    fn finished(&self) -> bool {
        matches!(self.phase, JobPhase::Done | JobPhase::Failed)
    }
}

#[derive(Clone, Serialize)]
pub struct CatalogEntry {
    pub id: &'static str,
    pub name: &'static str,
    pub file_name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
    pub license: &'static str,
    pub license_url: &'static str,
    pub source_url: &'static str,
    pub attribution: &'static str,
    /// Exact text the user accepts, and its digest to echo on import.
    pub terms: String,
    pub terms_sha256: String,
    /// The profile this build registers for the model.
    pub profile: &'static str,
    pub profile_version: u32,
    pub profile_sha256: String,
}
#[derive(Serialize)]
pub struct InstalledModel {
    pub id: String,
    pub sha256: String,
    pub size: u64,
    pub license: String,
    pub accepted_at: u64,
    /// Display name and profile, when the installed file is a model this
    /// build registers. A file of the same ID that is not that model, such as
    /// one a previous version shipped, has neither.
    pub name: Option<&'static str>,
    pub profile: Option<&'static str>,
    /// The installed file is a model this build registers and can serve.
    pub supported: bool,
    /// The stored consent covers this exact model and the current terms.
    pub consent_current: bool,
}
#[derive(Serialize)]
pub struct QualificationSummary {
    pub created_at: u64,
    /// The profile the counts were measured with.
    pub profile: String,
    pub profile_version: u32,
    #[serde(flatten)]
    pub measurement: Measurement,
}
#[derive(Serialize)]
pub struct Status {
    /// Model and runtime are qualified; `llm` may be switched on.
    pub ready: bool,
    pub enabled: bool,
    pub runtime: &'static str,
    pub state: Lifecycle,
    pub reason: Reason,
    pub catalog: Vec<CatalogEntry>,
    pub installed: Option<InstalledModel>,
    pub qualification: Option<QualificationSummary>,
    pub job: Option<JobStatus>,
}

/// A request names a registered model and nothing else about it. The profile
/// follows from the model; there is no field with which to choose one, and
/// unknown fields are refused.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportRequest {
    /// Absolute path of a file the user obtained.
    pub path: PathBuf,
    pub model: String,
    pub license: String,
    pub terms_sha256: String,
    pub accept_license: bool,
    pub accept_local_processing: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartResult {
    Started,
    Busy,
    Invalid,
    ConsentRequired,
    UnsupportedModel,
    NotInstalled,
    RuntimeMissing,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Started {
    pub result: StartResult,
    pub job: Option<u64>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoveResult {
    Removed,
    Busy,
    NotInstalled,
    Failed,
}

pub type Activation = Box<dyn FnOnce() -> std::io::Result<()> + Send>;

enum Task {
    Refresh,
    Import(u64, PathBuf, &'static Model, Option<Activation>),
    Qualify(u64, Option<Activation>),
}
struct State {
    enabled: bool,
    install: Option<Install>,
    /// The registered model the install record names, if this build has it.
    model: Option<&'static Model>,
    qualification: Recorded,
    worker_present: bool,
    loaded: bool,
    /// `Reason::None` means the installed model is qualified for this build.
    reason: Reason,
    serving: bool,
    job: Option<JobStatus>,
    next_job: u64,
    /// The engine has started a measurement by itself. It does so once per
    /// process, so a measurement that fails does not repeat itself.
    renewed: bool,
}
impl State {
    /// A job holds the one slot until it has finished.
    fn busy(&self) -> bool {
        self.job.is_some_and(|job| !job.finished())
    }
    /// Takes the slot for a new job, unless one is under way.
    fn reserve(&mut self, kind: JobKind) -> Option<u64> {
        if self.busy() {
            return None;
        }
        self.next_job += 1;
        self.job = Some(JobStatus {
            id: self.next_job,
            kind,
            phase: JobPhase::Queued,
            copied: 0,
            error: JobError::None,
        });
        Some(self.next_job)
    }
}
struct Inner {
    store: Store,
    worker: PathBuf,
    service: Service,
    state: Mutex<State>,
    tasks: Mutex<Option<SyncSender<Task>>>,
    /// Built on the first status request; digests are not recomputed per poll.
    catalog: OnceLock<Vec<CatalogEntry>>,
}

#[derive(Clone)]
pub struct Manager(Arc<Inner>);
impl Manager {
    /// `dir` holds the model and its records. Readiness is established on the
    /// management thread; until then nothing is served.
    pub fn open(dir: PathBuf, worker: PathBuf, enabled: bool) -> Self {
        let service = Service::new(ProcessSpawner {
            worker: worker.clone(),
        });
        let manager = Self::unread(service, dir, worker, enabled);
        manager.send(Task::Refresh);
        manager
    }
    /// A manager that has not read its records.
    fn unread(service: Service, dir: PathBuf, worker: PathBuf, enabled: bool) -> Self {
        Self(Arc::new(Inner {
            service,
            store: Store::new(dir),
            worker,
            state: Mutex::new(State {
                enabled,
                install: None,
                model: None,
                qualification: Recorded::Missing,
                worker_present: false,
                loaded: false,
                reason: Reason::ReportMissing,
                serving: false,
                job: None,
                next_job: 0,
                renewed: false,
            }),
            tasks: Mutex::new(None),
            catalog: OnceLock::new(),
        }))
    }
    pub fn service(&self) -> Service {
        self.0.service.clone()
    }
    /// Whether `llm` may be enabled right now.
    pub fn ready(&self) -> bool {
        let state = self.lock();
        ready(&state, self.0.service.health())
    }
    /// Applies the user setting. Starts or stops the worker; no file access.
    pub fn set_enabled(&self, enabled: bool) {
        let mut state = self.lock();
        state.enabled = enabled;
        let renewal = self.settle(&mut state);
        drop(state);
        self.dispatch(renewal);
    }
    pub fn status(&self) -> Status {
        let catalog = self
            .0
            .catalog
            .get_or_init(|| {
                profile::models()
                    .map(|model| CatalogEntry {
                        id: model.id,
                        name: model.name,
                        file_name: model.file_name,
                        size: model.size,
                        sha256: model.sha256,
                        license: model.license,
                        license_url: model.license_url,
                        source_url: model.source_url,
                        attribution: model.attribution,
                        terms: profile::terms(model),
                        terms_sha256: profile::terms_sha256(model),
                        profile: model.profile.id,
                        profile_version: model.profile.version,
                        profile_sha256: model.profile.sha256(),
                    })
                    .collect()
            })
            .clone();
        let state = self.lock();
        let health = self.0.service.health();
        let running = state.busy();
        let (lifecycle, reason) = match (&state.install, health) {
            _ if !state.loaded => (Lifecycle::Loading, Reason::None),
            (None, _) => (Lifecycle::NotInstalled, Reason::None),
            (Some(_), _) if running => (Lifecycle::Qualifying, Reason::None),
            (Some(_), Health::Faulted(reason)) => (Lifecycle::Faulted, reason),
            (Some(_), _) if state.reason == Reason::None => (Lifecycle::Qualified, Reason::None),
            (Some(_), _) => (Lifecycle::Installed, state.reason),
        };
        Status {
            ready: ready(&state, health),
            enabled: state.enabled,
            runtime: if state.worker_present {
                "available"
            } else {
                "missing"
            },
            state: lifecycle,
            reason,
            catalog,
            installed: state.install.as_ref().map(|install| {
                // The registered model of that ID, only if the file is it.
                let model = state.model.filter(|model| install.names(model));
                InstalledModel {
                    id: install.model.clone(),
                    sha256: install.sha256.clone(),
                    size: install.size,
                    license: install.license.clone(),
                    accepted_at: install.accepted_at,
                    name: model.map(|model| model.name),
                    profile: model.map(|model| model.profile.id),
                    supported: model.is_some(),
                    consent_current: model.is_some_and(|model| install.consents(model)),
                }
            }),
            qualification: state
                .qualification
                .bound()
                .map(|qualification| QualificationSummary {
                    created_at: qualification.evidence.created_at,
                    profile: qualification.evidence.profile.clone(),
                    profile_version: qualification.evidence.profile_version,
                    measurement: qualification.evidence.measurement,
                }),
            job: state.job,
        }
    }
    /// Starts an import after checking the explicit consent. The file is only
    /// read on the management thread.
    pub fn import(&self, request: ImportRequest) -> Started {
        self.import_with_activation(request, None)
    }
    /// The activation runs on the management thread after successful
    /// qualification, while this job still owns the slot.
    pub fn import_with_activation(
        &self,
        request: ImportRequest,
        activation: Option<Activation>,
    ) -> Started {
        let refuse = |result| Started { result, job: None };
        let Some(model) = profile::model(&request.model) else {
            return refuse(StartResult::UnsupportedModel);
        };
        if !request.path.is_absolute() {
            return refuse(StartResult::Invalid);
        }
        if !request.accept_license
            || !request.accept_local_processing
            || request.license != model.license
            || request.terms_sha256 != profile::terms_sha256(model)
        {
            return refuse(StartResult::ConsentRequired);
        }
        self.start(JobKind::Import, |id| {
            Task::Import(id, request.path, model, activation)
        })
    }
    /// Measures the installed model again with its registered profile, for
    /// example after a worker update or when the stored measurement is not
    /// bound to the current profile.
    pub fn qualify(&self) -> Started {
        self.qualify_with_activation(None)
    }
    pub fn qualify_with_activation(&self, activation: Option<Activation>) -> Started {
        {
            let state = self.lock();
            let refusal = match (&state.install, state.model, state.reason) {
                (None, _, _) => Some(StartResult::NotInstalled),
                (Some(_), None, _) => Some(StartResult::UnsupportedModel),
                (Some(_), Some(_), Reason::ConsentChanged) => Some(StartResult::ConsentRequired),
                _ => None,
            };
            if let Some(result) = refusal {
                return Started { result, job: None };
            }
        }
        self.start(JobKind::Qualify, |id| Task::Qualify(id, activation))
    }
    /// Stops the service and deletes the model with all of its records.
    pub fn remove(&self) -> RemoveResult {
        let mut state = self.lock();
        if state.busy() {
            return RemoveResult::Busy;
        }
        let installed = state.install.is_some();
        self.0.service.deactivate();
        state.serving = false;
        state.install = None;
        state.model = None;
        state.qualification = Recorded::Missing;
        state.reason = Reason::ReportMissing;
        state.job = None;
        // The caller switches the setting off next. Until it has, and if it
        // cannot, the model is switched on with nothing left to serve.
        self.announce(&state);
        if self.0.store.remove().is_err() {
            return RemoveResult::Failed;
        }
        if installed {
            RemoveResult::Removed
        } else {
            RemoveResult::NotInstalled
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn start(&self, kind: JobKind, task: impl FnOnce(u64) -> Task) -> Started {
        let reserved = {
            let mut state = self.lock();
            let reserved = state.reserve(kind);
            if reserved.is_some() {
                // The settings show the job; nothing more is asked of the user.
                self.announce(&state);
            }
            reserved
        };
        let Some(id) = reserved else {
            return Started {
                result: StartResult::Busy,
                job: None,
            };
        };
        if self.send(task(id)) {
            Started {
                result: StartResult::Started,
                job: Some(id),
            }
        } else {
            self.finish(id, JobError::WorkerFailed);
            Started {
                result: StartResult::Busy,
                job: None,
            }
        }
    }
    fn send(&self, task: Task) -> bool {
        let mut tasks = self.0.tasks.lock().unwrap_or_else(|e| e.into_inner());
        if tasks.is_none() {
            let (sender, receiver) = mpsc::sync_channel::<Task>(4);
            let manager = self.clone();
            let spawned = std::thread::Builder::new()
                .name("kanade-llm-manage".into())
                .spawn(move || {
                    while let Ok(task) = receiver.recv() {
                        match task {
                            Task::Refresh => manager.refresh(),
                            Task::Import(id, path, model, activation) => {
                                manager.run_import(id, &path, model, activation)
                            }
                            Task::Qualify(id, activation) => manager.run_qualify(id, activation),
                        }
                    }
                });
            if spawned.is_err() {
                return false;
            }
            *tasks = Some(sender);
        }
        tasks
            .as_ref()
            .is_some_and(|sender| sender.try_send(task).is_ok())
    }
    fn phase(&self, id: u64, phase: JobPhase, copied: Option<u8>) {
        let mut state = self.lock();
        if let Some(job) = state.job.as_mut().filter(|job| job.id == id) {
            job.phase = phase;
            if let Some(copied) = copied {
                job.copied = copied;
            }
        }
    }
    /// Ends a job. A job that ran has read the records again first, so the
    /// state is what it left.
    fn finish(&self, id: u64, error: JobError) {
        let mut state = self.lock();
        if let Some(job) = state.job.as_mut().filter(|job| job.id == id) {
            job.error = error;
            job.phase = if error == JobError::None {
                JobPhase::Done
            } else {
                JobPhase::Failed
            };
        }
        // With the slot free, a measurement that had to wait for this job is
        // reserved, or else the user is told that nothing is served.
        let renewal = self.settle(&mut state);
        drop(state);
        self.dispatch(renewal);
    }
    /// Brings the service in line with the state and reserves the measurement
    /// the engine owes, in one step: no reader sees the stale model without
    /// the job that renews it, and the key loop is not told meanwhile that
    /// the user is needed. The caller passes the result to `dispatch` once
    /// the state is unlocked.
    fn settle(&self, state: &mut State) -> Option<u64> {
        let renewal = if renewal_owed(state) {
            state.reserve(JobKind::Qualify)
        } else {
            None
        };
        state.renewed |= renewal.is_some();
        self.serve(state);
        renewal
    }
    /// Hands a measurement reserved by `settle` to the management thread.
    fn dispatch(&self, renewal: Option<u64>) {
        if let Some(id) = renewal
            && !self.send(Task::Qualify(id, None))
        {
            self.finish(id, JobError::WorkerFailed);
        }
    }
    /// Starts or stops serving to match the setting and the current verdict.
    fn serve(&self, state: &mut State) {
        let model = state
            .model
            .filter(|_| state.enabled && state.install.is_some() && state.reason == Reason::None);
        match model {
            Some(model) if !state.serving => {
                self.0.service.activate(self.0.store.model_path(), model)
            }
            None if state.serving => self.0.service.deactivate(),
            _ => {}
        }
        state.serving = model.is_some();
        self.announce(state);
    }
    /// Tells the key loop whether the user is needed in the settings: the
    /// model is switched on and nothing is served, nor is a job under way
    /// that may still make it servable.
    fn announce(&self, state: &State) {
        self.0
            .service
            .set_setup_required(state.enabled && state.loaded && !state.serving && !state.busy());
    }
    /// Re-reads the records and the worker binary and derives readiness.
    fn refresh(&self) {
        let renewal = self.reload();
        self.dispatch(renewal);
    }
    /// `refresh` up to the point where a reserved measurement is handed over.
    fn reload(&self) -> Option<u64> {
        let install = self.0.store.install();
        let qualification = self.0.store.qualification();
        let worker = store::sha256_file(&self.0.worker).ok();
        // The profile is the one registered for the installed model. An
        // unknown model has none and is never served.
        let model = install
            .as_ref()
            .and_then(|install| profile::model(&install.model));
        let reason = match (&install, model) {
            (None, _) => Reason::ReportMissing,
            (Some(_), None) => Reason::ModelUnsupported,
            (Some(install), Some(model)) => {
                if !install.names(model) {
                    Reason::ModelChanged
                } else if !install.consents(model) {
                    Reason::ConsentChanged
                } else if let Some(worker) = &worker {
                    match (self.0.store.model_reason(install), &qualification) {
                        (Reason::None, Recorded::Bound(qualification)) => {
                            qualification.verdict(install, worker)
                        }
                        // Measured before profiles existed: bound to none.
                        (Reason::None, Recorded::Unbound) => Reason::ProfileChanged,
                        (Reason::None, Recorded::Missing) => Reason::ReportMissing,
                        (reason, _) => reason,
                    }
                } else {
                    Reason::WorkerMissing
                }
            }
        };
        let mut state = self.lock();
        let changed = state.install != install || state.qualification != qualification;
        state.install = install;
        state.model = model;
        state.qualification = qualification;
        state.worker_present = worker.is_some();
        state.loaded = true;
        state.reason = reason;
        if changed && state.serving {
            // Never keep serving a model whose records changed underneath.
            self.0.service.deactivate();
            state.serving = false;
        }
        self.settle(&mut state)
    }
    fn run_import(
        &self,
        id: u64,
        source: &Path,
        model: &'static Model,
        activation: Option<Activation>,
    ) {
        // The previous model must not be scored while it is being replaced,
        // and its verdict says nothing about the files until they are read
        // again.
        {
            let mut state = self.lock();
            self.0.service.deactivate();
            state.serving = false;
            state.reason = Reason::ReportMissing;
        }
        self.phase(id, JobPhase::Copying, Some(0));
        let mut last = 0;
        let imported = self
            .0
            .store
            .import(source, model, |copied| {
                let percent = (copied.saturating_mul(100) / model.size.max(1)).min(100) as u8;
                if percent != last {
                    last = percent;
                    self.phase(id, JobPhase::Copying, Some(percent));
                }
            })
            .map_err(|error| match error {
                ImportError::Source => JobError::SourceUnreadable,
                ImportError::Mismatch => JobError::ModelMismatch,
                ImportError::Storage => JobError::Storage,
            })
            .and_then(|install| {
                self.0
                    .store
                    .save_install(&install)
                    .map_err(|_| JobError::Storage)
            });
        if let Err(error) = imported {
            // The attempt may have replaced the file or removed the records.
            // Read what it left before the job ends.
            self.refresh();
            return self.finish(id, error);
        }
        self.phase(id, JobPhase::Copying, Some(100));
        self.refresh();
        self.run_qualify(id, activation);
    }
    fn run_qualify(&self, id: u64, activation: Option<Activation>) {
        let mut error = self.measure(id).err().unwrap_or(JobError::None);
        self.refresh();
        if error == JobError::None
            && let Some(activate) = activation
        {
            self.phase(id, JobPhase::Activating, None);
            let qualified = self.lock().reason == Reason::None;
            if !qualified || activate().is_err() {
                error = JobError::ActivationFailed;
            }
        }
        self.finish(id, error);
    }
    fn measure(&self, id: u64) -> Result<(), JobError> {
        let install = self.0.store.install().ok_or(JobError::NotInstalled)?;
        let model = profile::model(&install.model).ok_or(JobError::ModelMismatch)?;
        if !install.names(model) {
            return Err(JobError::ModelMismatch);
        }
        if !install.consents(model) {
            return Err(JobError::ConsentRequired);
        }
        match self.0.store.model_reason(&install) {
            Reason::None => {}
            Reason::ModelMissing => return Err(JobError::NotInstalled),
            _ => return Err(JobError::ModelMismatch),
        }
        {
            // One model in memory at a time, and no stale verdict while measuring.
            let mut state = self.lock();
            self.0.service.deactivate();
            state.serving = false;
            state.reason = Reason::ReportMissing;
        }
        self.0.store.clear_qualification();
        let worker_sha256 =
            store::sha256_file(&self.0.worker).map_err(|_| JobError::WorkerMissing)?;
        let measurement =
            qualify::run(&self.0.worker, &self.0.store.model_path(), model, |phase| {
                self.phase(
                    id,
                    match phase {
                        Phase::StartingRuntime => JobPhase::StartingRuntime,
                        Phase::Tokenizer => JobPhase::Tokenizer,
                        Phase::Bounds => JobPhase::Bounds,
                        Phase::Ranking => JobPhase::Ranking,
                    },
                    None,
                )
            })
            .map_err(|refusal| match refusal {
                Refusal::Identity => JobError::IdentityMismatch,
                Refusal::Worker(_) | Refusal::Corpus => JobError::WorkerFailed,
            })?;
        let profile = model.profile;
        // The record is kept even when a gate fails, so the status can show
        // the measured counts. Readiness is derived from them, not from a flag.
        let qualification = Evidence::new(model, worker_sha256, measurement).seal();
        self.0
            .store
            .save_qualification(&qualification)
            .map_err(|_| JobError::Storage)?;
        match measurement.verdict(profile) {
            Reason::None => Ok(()),
            Reason::TokenizerMismatch => Err(JobError::TokenizerMismatch),
            _ => Err(JobError::QualityGate),
        }
    }
}
fn ready(state: &State, health: Health) -> bool {
    state.install.is_some()
        && state.model.is_some()
        && state.worker_present
        && state.reason == Reason::None
        && !matches!(health, Health::Faulted(_))
        && !state.busy()
}
/// Whether the engine owes a measurement that nobody asked for.
///
/// The stored one is intact and names the installed file, but another worker
/// or runtime made it. An update of Kanade leaves exactly that: every release
/// builds the worker again. The reason does not say that nothing else of the
/// build changed. It is found before the profile, the prompt and the corpora
/// are compared, so those may differ as well. That is no obstacle. The engine
/// measures with the profile this build registers, as it would if the user
/// asked for it; the file is still the registered one and the consent still
/// covers the current terms, or the reason would be another; and a
/// measurement reads no text of the user and stays on this computer.
///
/// A measurement that ends, passed or failed, leaves another reason. The
/// engine starts one once per process even so, whatever a later reading of
/// the records says.
fn renewal_owed(state: &State) -> bool {
    state.loaded
        && state.enabled
        && !state.renewed
        && !state.busy()
        && state.reason == Reason::RuntimeChanged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Job, Rerank,
        process::Failure,
        profile::{CATALOG, fixture},
        service::{Backend, Spawner, StartError},
        store::{
            Stamp,
            tests::{legacy_install, legacy_qualification},
        },
    };
    use sha2::{Digest, Sha256};
    use std::time::{Duration, Instant};

    /// Stands in for the worker process that the service starts.
    enum Stub {
        /// Loads whatever it is given.
        Serving,
        /// Finds another file than the registered one.
        Refusing,
    }
    struct Flat;
    impl Backend for Flat {
        fn score(&mut self, job: &Job, _: bool) -> Result<Vec<Option<f32>>, Failure> {
            Ok(vec![Some(0.0); job.candidates.len()])
        }
    }
    impl Spawner for Stub {
        fn start(&self, _: &Path, _: &'static Model) -> Result<Box<dyn Backend>, StartError> {
            match self {
                Self::Serving => Ok(Box::new(Flat)),
                Self::Refusing => Err(StartError::Rejected(Reason::ModelChanged)),
            }
        }
    }
    /// Writes a file small enough to import in a test, and registers it.
    fn small(source: &Path) -> &'static Model {
        let bytes = b"stands in for a model file";
        std::fs::write(source, bytes).unwrap();
        let base = &CATALOG[0];
        Box::leak(Box::new(Model {
            id: "fixture-small",
            name: base.name,
            file_name: base.file_name,
            size: bytes.len() as u64,
            sha256: profile::hex(&Sha256::digest(bytes)).leak(),
            license: base.license,
            license_url: base.license_url,
            source_url: base.source_url,
            revision: base.revision,
            attribution: base.attribution,
            profile: base.profile,
            preview: base.preview,
        }))
    }

    /// A data directory with a stand-in model file and a stand-in worker file.
    /// Reading records never opens either of them as a model or a program.
    struct Fixture {
        _dir: tempfile::TempDir,
        data: PathBuf,
        worker: PathBuf,
        worker_sha256: String,
        stamp: Stamp,
    }
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("llm");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("model.gguf"), b"stand-in").unwrap();
        let worker = dir.path().join("worker");
        std::fs::write(&worker, b"not a program").unwrap();
        Fixture {
            worker_sha256: store::sha256_file(&worker).unwrap(),
            stamp: Stamp::of(&data.join("model.gguf")).unwrap(),
            _dir: dir,
            data,
            worker,
        }
    }
    impl Fixture {
        fn install(&self, model: &Model) {
            self.write("install.json", &legacy_install(model, self.stamp));
        }
        fn write(&self, name: &str, text: &str) {
            std::fs::write(self.data.join(name), text).unwrap();
        }
        fn qualified(&self, model: &Model) -> Evidence {
            let profile = model.profile;
            let count = qualify::ranking_count(profile);
            Evidence::new(
                model,
                self.worker_sha256.clone(),
                Measurement {
                    tokenizer_cases: qualify::parity_count(profile),
                    tokenizer_mismatches: 0,
                    ranking_cases: count,
                    ranking_top1: count,
                    ranking_unscored: 0,
                    unstable: 0,
                    bound_failures: 0,
                    p50_ms: 20,
                    p95_ms: 40,
                    max_ms: 60,
                },
            )
        }
        /// A passing measurement that the worker of another version made.
        fn stale(&self, model: &Model) -> Evidence {
            Evidence {
                worker_sha256: "0".repeat(64),
                ..self.qualified(model)
            }
        }
        fn record(&self, evidence: Evidence) {
            self.write(
                "qualification.json",
                &serde_json::to_string(&evidence.seal()).unwrap(),
            );
        }
        /// A manager that reads its records when the test calls `reload`.
        fn unread(&self, enabled: bool, worker: Stub) -> Manager {
            Manager::unread(
                Service::new(worker),
                self.data.clone(),
                self.worker.clone(),
                enabled,
            )
        }
        fn open(&self, enabled: bool) -> (Manager, Status) {
            let manager = Manager::open(self.data.clone(), self.worker.clone(), enabled);
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let status = manager.status();
                if status.state != Lifecycle::Loading {
                    return (manager, status);
                }
                assert!(Instant::now() < deadline, "records were not read");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
    fn finished(manager: &Manager) -> Status {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let status = manager.status();
            if status.job.is_some_and(|job| job.finished()) {
                return status;
            }
            assert!(Instant::now() < deadline, "management job did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn job() -> Job {
        Job {
            context: "今日は".into(),
            reading: "コウエン".into(),
            prefix: String::new(),
            suffix: String::new(),
            candidates: vec!["公園".into(), "講演".into()],
        }
    }

    #[test]
    fn records_from_before_profiles_keep_consent_and_need_a_new_qualification() {
        let tsumugi = &CATALOG[0];
        let f = fixture();
        f.install(tsumugi);
        f.write("qualification.json", &legacy_qualification(tsumugi));
        let install_before = std::fs::read(f.data.join("install.json")).unwrap();

        // The model had been switched on. It is not served, and not ready.
        let (manager, status) = f.open(true);
        assert_eq!(
            (status.state, status.reason, status.ready),
            (Lifecycle::Installed, Reason::ProfileChanged, false)
        );
        assert!(!manager.ready() && status.enabled);
        let installed = status.installed.as_ref().unwrap();
        assert_eq!(installed.id, tsumugi.id);
        assert_eq!(
            (installed.name, installed.profile, installed.supported),
            (Some(tsumugi.name), Some(tsumugi.profile.id), true)
        );
        assert_eq!(installed.accepted_at, 1_700_000_000);
        assert!(status.qualification.is_none());
        let service = manager.service();
        assert!(!service.active());
        assert_eq!(service.submit(job()), None);

        // Qualifying again is offered and runs. With no real worker it fails,
        // and nothing becomes ready or served on the way.
        let started = manager.qualify();
        assert_eq!(started.result, StartResult::Started);
        let status = finished(&manager);
        assert_eq!(status.job.unwrap().error, JobError::WorkerFailed);
        assert_eq!(
            (status.state, status.reason, status.ready),
            (Lifecycle::Installed, Reason::ReportMissing, false)
        );
        assert!(!service.active() && service.scored() == 0);
        // The consent record was not touched by any of this.
        assert_eq!(
            std::fs::read(f.data.join("install.json")).unwrap(),
            install_before
        );
    }
    #[test]
    fn a_measurement_by_another_worker_is_renewed_once_without_being_asked() {
        let tsumugi = &CATALOG[0];
        // What an update leaves behind: another worker made the measurement.
        // The rest of the build may differ as well, here the profile version
        // and the prompt. The reason is the same, and so is what follows.
        type Edit = fn(&mut Evidence);
        let updates: [Edit; 2] = [
            |_| {},
            |evidence| {
                evidence.profile_version += 1;
                evidence.prompt_sha256 = "1".repeat(64);
            },
        ];
        for edit in updates {
            let f = fixture();
            f.install(tsumugi);
            let mut measured = f.stale(tsumugi);
            edit(&mut measured);
            f.record(measured.clone());
            // Switched off: nothing is measured, and nothing is asked.
            let (manager, status) = f.open(false);
            assert_eq!((status.reason, status.job), (Reason::RuntimeChanged, None));
            assert!(!Rerank::setup_required(&manager.service()));

            // Switched on. Reading the records reserves the measurement in
            // the same step: the stale model is never seen without its job,
            // and the key loop is not told that the user is needed.
            let manager = f.unread(true, Stub::Serving);
            let renewal = manager.reload();
            let status = manager.status();
            let job = status.job.unwrap();
            assert_eq!(renewal, Some(job.id));
            assert_eq!((job.kind, job.phase), (JobKind::Qualify, JobPhase::Queued));
            assert_eq!((status.state, status.ready), (Lifecycle::Qualifying, false));
            assert!(!Rerank::setup_required(&manager.service()));
            // With no real worker the measurement fails. Now the user is
            // needed.
            manager.dispatch(renewal);
            let status = finished(&manager);
            let failed = status.job.unwrap();
            assert_eq!((failed.id, failed.error), (job.id, JobError::WorkerFailed));
            assert_eq!(status.reason, Reason::ReportMissing);
            assert!(Rerank::setup_required(&manager.service()));
            // The engine starts no second one, whatever the records say.
            f.record(measured);
            assert_eq!(manager.reload(), None);
            let status = manager.status();
            assert_eq!(status.reason, Reason::RuntimeChanged);
            assert_eq!(status.job.unwrap().id, job.id);
            assert!(Rerank::setup_required(&manager.service()));
        }
    }
    #[test]
    fn switching_a_stale_model_on_starts_its_measurement() {
        let tsumugi = &CATALOG[0];
        let f = fixture();
        f.install(tsumugi);
        f.record(f.stale(tsumugi));
        let (manager, status) = f.open(false);
        assert_eq!((status.reason, status.job), (Reason::RuntimeChanged, None));
        manager.set_enabled(true);
        // Reserved before the call returns; no status shows the model
        // switched on, stale and idle.
        assert_eq!(manager.status().job.unwrap().kind, JobKind::Qualify);
        assert_eq!(
            finished(&manager).job.unwrap().error,
            JobError::WorkerFailed
        );
    }
    #[test]
    fn a_renewal_waits_for_a_job_that_was_accepted_before_the_records_were_read() {
        let tsumugi = &CATALOG[0];
        let f = fixture();
        f.install(tsumugi);
        f.record(f.stale(tsumugi));
        let manager = f.unread(true, Stub::Serving);
        // An import is accepted while the records are still unread.
        let import = manager.lock().reserve(JobKind::Import).unwrap();
        // Reading them finds the slot taken. The measurement stays owed, and
        // the user is not sent to the settings for it.
        assert_eq!(manager.reload(), None);
        assert_eq!(manager.status().job.unwrap().id, import);
        assert!(!Rerank::setup_required(&manager.service()));
        // The import cannot read its source and leaves the records as they
        // were. The measurement follows without being asked for.
        manager.run_import(import, &f.data.join("absent.gguf"), tsumugi, None);
        let status = finished(&manager);
        let job = status.job.unwrap();
        assert_eq!(
            (job.id, job.kind, job.error),
            (import + 1, JobKind::Qualify, JobError::WorkerFailed)
        );
        assert!(Rerank::setup_required(&manager.service()));
    }
    #[test]
    fn a_failed_import_serves_only_what_the_records_still_qualify() {
        let tsumugi = &CATALOG[0];
        let f = fixture();
        f.install(tsumugi);
        f.record(f.qualified(tsumugi));
        let manager = f.unread(true, Stub::Serving);
        assert_eq!(manager.reload(), None);
        let service = manager.service();
        assert!(service.active());

        // A source that cannot be read leaves the installed model as it was.
        // It is stopped for the attempt and served again.
        let before = service.generation();
        let id = manager.lock().reserve(JobKind::Import).unwrap();
        manager.run_import(
            id,
            &f.data.join("absent.gguf"),
            tsumugi,
            Some(Box::new(|| panic!("failed import must not activate"))),
        );
        let status = manager.status();
        assert_eq!(status.job.unwrap().error, JobError::SourceUnreadable);
        assert_eq!((status.state, status.ready), (Lifecycle::Qualified, true));
        assert!(service.active());
        assert_eq!(service.generation(), before + 2);
        assert!(!Rerank::setup_required(&service));

        // This import replaces the file and removes the records, and then
        // cannot write the new record. The verdict from before the import
        // describes none of that: the service is stopped once and is not
        // started on it again, not even until the records are read.
        let source = f.data.join("source.gguf");
        let replacement = small(&source);
        std::fs::create_dir(f.data.join("install.next")).unwrap();
        let before = service.generation();
        let id = manager.lock().reserve(JobKind::Import).unwrap();
        manager.run_import(id, &source, replacement, None);
        let status = manager.status();
        assert_eq!(status.job.unwrap().error, JobError::Storage);
        assert_eq!(
            (status.state, status.ready),
            (Lifecycle::NotInstalled, false)
        );
        assert!(!service.active());
        assert_eq!(service.generation(), before + 1);
        assert_eq!(service.submit(job()), None);
        // Still switched on, and only the user can change what is served.
        assert!(Rerank::setup_required(&service));
    }
    #[test]
    fn a_switched_on_model_that_cannot_be_served_asks_for_the_settings() {
        let tsumugi = &CATALOG[0];
        let f = fixture();
        // The file of the previous version's model under the same ID.
        let mut install: serde_json::Value =
            serde_json::from_str(&legacy_install(tsumugi, f.stamp)).unwrap();
        install["terms_sha256"] = profile::terms_sha256(tsumugi).into();
        install["sha256"] = "1".repeat(64).into();
        f.write("install.json", &install.to_string());
        f.record(f.qualified(tsumugi));
        // Only an import helps. The engine measures nothing by itself.
        let (manager, status) = f.open(true);
        assert_eq!((status.reason, status.job), (Reason::ModelChanged, None));
        assert!(Rerank::setup_required(&manager.service()));
        drop(manager);
        let (manager, _) = f.open(false);
        assert!(!Rerank::setup_required(&manager.service()));
        drop(manager);
        // Neither does it for a record that names this worker and another
        // profile: no update of Kanade leaves that.
        f.install(tsumugi);
        let mut other = f.qualified(tsumugi);
        other.profile_version += 1;
        f.record(other);
        let (manager, status) = f.open(true);
        assert_eq!((status.reason, status.job), (Reason::ProfileChanged, None));
        assert!(Rerank::setup_required(&manager.service()));
        drop(manager);
        // A model that is served needs nothing.
        f.record(f.qualified(tsumugi));
        let manager = f.unread(true, Stub::Serving);
        assert_eq!(manager.reload(), None);
        assert_eq!(manager.status().reason, Reason::None);
        assert!(manager.service().active());
        assert!(!Rerank::setup_required(&manager.service()));
    }
    #[test]
    fn a_worker_that_refuses_the_qualified_model_asks_for_the_settings() {
        let tsumugi = &CATALOG[0];
        let f = fixture();
        f.install(tsumugi);
        f.record(f.qualified(tsumugi));
        // The records qualify the model, so the manager serves it.
        let manager = f.unread(true, Stub::Refusing);
        assert_eq!(manager.reload(), None);
        let service = manager.service();
        assert!(service.active());
        // The worker finds another file than the registered one. The service
        // stops and is not started again.
        let deadline = Instant::now() + Duration::from_secs(10);
        while manager.status().state != Lifecycle::Faulted {
            assert!(Instant::now() < deadline, "the service did not stop");
            std::thread::sleep(Duration::from_millis(2));
        }
        let status = manager.status();
        assert_eq!((status.reason, status.ready), (Reason::ModelChanged, false));
        assert_eq!(service.submit(job()), None);
        assert!(Rerank::setup_required(&service));
        // Switched off, nothing is asked.
        manager.set_enabled(false);
        assert!(!Rerank::setup_required(&service));
    }
    #[test]
    fn removing_a_model_that_stays_switched_on_asks_for_the_settings() {
        let tsumugi = &CATALOG[0];
        let f = fixture();
        f.install(tsumugi);
        f.record(f.qualified(tsumugi));
        let manager = f.unread(true, Stub::Serving);
        assert_eq!(manager.reload(), None);
        let service = manager.service();
        assert!(service.active() && !Rerank::setup_required(&service));
        // Removal leaves the setting to its caller. Until that has switched
        // it off, and when saving the setting fails, the model is switched on
        // with nothing to serve.
        assert_eq!(manager.remove(), RemoveResult::Removed);
        assert!(!service.active());
        assert!(Rerank::setup_required(&service));
        manager.set_enabled(false);
        assert!(!Rerank::setup_required(&service));
    }
    #[test]
    fn readiness_follows_the_profile_registered_for_the_installed_model() {
        let tsumugi = &CATALOG[0];
        let alt = fixture::alt();
        let f = fixture();
        f.install(tsumugi);
        f.record(f.qualified(tsumugi));
        let (manager, status) = f.open(false);
        assert_eq!(
            (status.state, status.reason, status.ready),
            (Lifecycle::Qualified, Reason::None, true)
        );
        let summary = status.qualification.unwrap();
        assert_eq!(
            (summary.profile.as_str(), summary.profile_version),
            (tsumugi.profile.id, tsumugi.profile.version)
        );
        // Ready, but not enabled: nothing is served.
        assert!(!manager.service().active());
        drop(manager);

        // A passing record of another profile under the installed model's name.
        let mut wrong = f.qualified(alt);
        wrong.model = tsumugi.id.into();
        f.record(wrong);
        let (_, status) = f.open(true);
        assert_eq!(
            (status.state, status.reason, status.ready),
            (Lifecycle::Installed, Reason::ProfileChanged, false)
        );
        // The same file installed as another model: the record measured as
        // the first one does not carry over, in either direction.
        f.record(f.qualified(tsumugi));
        let mut install: serde_json::Value =
            serde_json::from_str(&legacy_install(alt, f.stamp)).unwrap();
        install["terms_sha256"] = profile::terms_sha256(tsumugi).into();
        f.write("install.json", &install.to_string());
        // Consent recorded for the first model cannot authorize the second.
        let (manager, status) = f.open(true);
        assert_eq!(status.reason, Reason::ConsentChanged);
        assert_eq!(manager.qualify().result, StartResult::ConsentRequired);
        install["terms_sha256"] = profile::terms_sha256(alt).into();
        f.write("install.json", &install.to_string());
        let (_, status) = f.open(true);
        assert_eq!((status.reason, status.ready), (Reason::ModelChanged, false));
        assert_eq!(status.installed.unwrap().profile, Some(alt.profile.id));
        f.record(f.qualified(alt));
        let (_, status) = f.open(false);
        assert_eq!((status.reason, status.ready), (Reason::None, true));
        assert_eq!(status.qualification.unwrap().profile, alt.profile.id);
    }
    #[test]
    fn unknown_models_changed_terms_and_other_files_fail_closed() {
        let tsumugi = &CATALOG[0];
        let f = fixture();
        // A model this build does not register, with a record that claims a
        // complete, passing measurement for it.
        let mut install: serde_json::Value =
            serde_json::from_str(&legacy_install(tsumugi, f.stamp)).unwrap();
        install["model"] = "unregistered-q5km".into();
        f.write("install.json", &install.to_string());
        let mut claimed = f.qualified(tsumugi);
        claimed.model = "unregistered-q5km".into();
        claimed.profile = "unregistered".into();
        f.record(claimed);
        let (manager, status) = f.open(true);
        assert_eq!(
            (status.state, status.reason, status.ready),
            (Lifecycle::Installed, Reason::ModelUnsupported, false)
        );
        let installed = status.installed.unwrap();
        assert_eq!(
            (installed.name, installed.profile, installed.supported),
            (None, None, false)
        );
        assert_eq!(manager.qualify().result, StartResult::UnsupportedModel);
        assert!(!manager.service().active());
        // It can still be removed.
        assert_eq!(manager.remove(), RemoveResult::Removed);
        assert_eq!(manager.status().state, Lifecycle::NotInstalled);

        let f = fixture();
        // Terms other than the ones that were accepted.
        let mut install: serde_json::Value =
            serde_json::from_str(&legacy_install(tsumugi, f.stamp)).unwrap();
        install["terms_sha256"] = "1".repeat(64).into();
        f.write("install.json", &install.to_string());
        f.record(f.qualified(tsumugi));
        let (manager, status) = f.open(true);
        assert_eq!(
            (status.reason, status.ready),
            (Reason::ConsentChanged, false)
        );
        assert_eq!(manager.qualify().result, StartResult::ConsentRequired);
        assert!(!manager.service().active());
        // A record for a file other than the registered one.
        install["terms_sha256"] = profile::terms_sha256(tsumugi).into();
        install["sha256"] = "1".repeat(64).into();
        f.write("install.json", &install.to_string());
        let (manager, status) = f.open(true);
        assert_eq!((status.reason, status.ready), (Reason::ModelChanged, false));
        // The file is not the registered model of its ID: it carries neither
        // that model's name nor its profile.
        let installed = status.installed.as_ref().unwrap();
        assert_eq!(
            (installed.name, installed.profile, installed.supported),
            (None, None, false)
        );
        assert_eq!(manager.qualify().result, StartResult::Started);
        assert_eq!(
            finished(&manager).job.unwrap().error,
            JobError::ModelMismatch
        );
    }
    #[test]
    fn an_import_names_a_registered_model_and_cannot_choose_a_profile() {
        let tsumugi = &CATALOG[0];
        let f = fixture();
        let (manager, _) = f.open(false);
        let request = |model: &str| ImportRequest {
            path: f.data.join("absent.gguf"),
            model: model.into(),
            license: tsumugi.license.into(),
            terms_sha256: profile::terms_sha256(tsumugi),
            accept_license: true,
            accept_local_processing: true,
        };
        // Neither an unregistered model nor a profile ID is a model.
        for model in [
            "unregistered-q5km",
            "unsupported-profile",
            "fixture-alt",
            "",
        ] {
            assert_eq!(
                manager.import(request(model)).result,
                StartResult::UnsupportedModel
            );
        }
        // Consent to one model's terms is not consent to another model, even
        // for the same file.
        assert_eq!(
            manager.import(request(fixture::alt().id)).result,
            StartResult::ConsentRequired
        );
        // The registered model proceeds to reading the file.
        assert_eq!(
            manager.import(request(tsumugi.id)).result,
            StartResult::Started
        );
        assert_eq!(
            finished(&manager).job.unwrap().error,
            JobError::SourceUnreadable
        );
        // The request format has no way to state a profile, a prompt, a
        // manifest or a result.
        let json = serde_json::json!({
            "path": "/models/model.gguf", "model": tsumugi.id, "license": tsumugi.license,
            "terms_sha256": "0", "accept_license": true, "accept_local_processing": true,
        });
        assert!(serde_json::from_value::<ImportRequest>(json.clone()).is_ok());
        for (key, value) in [
            ("profile", "fixture-alt".into()),
            ("prompt", "x".into()),
            ("manifest", serde_json::json!({"end": 2})),
            ("qualified", true.into()),
        ] {
            let mut extended = json.clone();
            extended[key] = value;
            assert!(serde_json::from_value::<ImportRequest>(extended).is_err());
        }
    }
    #[test]
    fn status_lists_identities_without_paths_or_text() {
        let tsumugi = &CATALOG[0];
        let f = fixture();
        f.install(tsumugi);
        f.record(f.qualified(tsumugi));
        let (_, status) = f.open(false);
        let json = serde_json::to_value(&status).unwrap();
        let entry = json["catalog"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == tsumugi.id)
            .unwrap();
        for (key, value) in [
            ("name", tsumugi.name.into()),
            ("size", tsumugi.size.into()),
            ("license", tsumugi.license.into()),
            ("license_url", tsumugi.license_url.into()),
            ("source_url", tsumugi.source_url.into()),
            ("attribution", tsumugi.attribution.into()),
            ("terms_sha256", profile::terms_sha256(tsumugi).into()),
            ("profile", tsumugi.profile.id.into()),
            ("profile_version", tsumugi.profile.version.into()),
            ("profile_sha256", tsumugi.profile.sha256().into()),
        ] {
            let value: serde_json::Value = value;
            assert_eq!(entry[key], value, "{key}");
        }
        assert_eq!(json["installed"]["id"], tsumugi.id);
        assert_eq!(json["installed"]["profile"], tsumugi.profile.id);
        assert_eq!(json["qualification"]["profile"], tsumugi.profile.id);
        assert_eq!(json["qualification"]["ranking_cases"], 85);
        assert_eq!(json["state"], "qualified");
        assert_eq!(json["reason"], "none");
        for reason in [
            Reason::ProfileChanged,
            Reason::ModelUnsupported,
            Reason::ConsentChanged,
        ] {
            let name = serde_json::to_value(reason).unwrap();
            assert!(
                ["profile_changed", "model_unsupported", "consent_changed"]
                    .contains(&name.as_str().unwrap())
            );
        }
        // No location of the model, the records or the worker.
        let text = json.to_string();
        assert!(!text.contains(f.data.to_str().unwrap()));
        assert!(!text.contains("model.gguf") && !text.contains("\"path\""));
    }
}
