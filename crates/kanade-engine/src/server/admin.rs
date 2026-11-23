use super::*;
use kanade_learn::service::{AdminAction, LearningService};
use kanade_llm::manager::{Activation, ImportRequest, Started};
use serde::Deserialize;
use std::{
    path::PathBuf,
    sync::{Mutex, OnceLock},
};
pub(super) struct Administration {
    data: PathBuf,
    settings: Mutex<Settings>,
    /// `None` when the learning store could not be read at start.
    learning: Option<LearningService>,
    project_reload: Arc<Mutex<Arc<kanade_indexer::Catalog>>>,
    project_jobs: OnceLock<io::Result<Arc<kanade_indexer::workflow::Service>>>,
    project_monitor: OnceLock<io::Result<kanade_indexer::monitor::Manager>>,
    dictionary: OnceLock<io::Result<dictionary::Service>>,
    llm: kanade_llm::Manager,
}
/// The settings in force.
struct Settings {
    config: config::Config,
    /// The settings file exists but could not be read, so `config` is
    /// `Config::unreadable()`. Nothing writes the file while this holds: it may
    /// carry settings from a newer version, or a repair in progress.
    unreadable: bool,
}
impl Settings {
    fn state(&self) -> StoreState {
        if self.unreadable {
            StoreState::Unreadable
        } else {
            StoreState::Read
        }
    }
}
#[derive(Deserialize)]
#[serde(untagged)]
enum SettingsRequest {
    Edit(SettingsChanges),
    Replace(config::Config),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsChanges {
    changes: serde_json::Map<String, serde_json::Value>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum ModelImport {
    Activate(ImportAndActivate),
    Import(ImportRequest),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportAndActivate {
    import_and_activate: ImportRequest,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ModelQualification {
    activate: bool,
}
impl Administration {
    /// `settings` is `None` when the settings file could not be read.
    pub(super) fn new(
        data: PathBuf,
        settings: Option<config::Config>,
        learning: Option<LearningService>,
        projects: Arc<kanade_indexer::Catalog>,
        llm: kanade_llm::Manager,
    ) -> Self {
        Self {
            data,
            settings: Mutex::new(Settings {
                unreadable: settings.is_none(),
                config: settings.unwrap_or_else(config::Config::unreadable),
            }),
            learning,
            project_reload: Arc::new(Mutex::new(projects)),
            project_jobs: OnceLock::new(),
            project_monitor: OnceLock::new(),
            dictionary: OnceLock::new(),
            llm,
        }
    }
    /// Answers a request, with whether the settings file and the learning
    /// store could be read.
    pub(super) fn handle(
        self: &Arc<Self>,
        request: &Manage,
        tx: &SyncSender<Work>,
    ) -> io::Result<Management> {
        // Read under the same lock as the settings it describes, so settings
        // kept off for a file that could not be read never pass as read.
        let mut settings_state = None;
        use manage::Action;
        let action = Action::try_from(request.action).map_err(|_| io::ErrorKind::InvalidInput)?;
        let value = match action {
            Action::ProjectMonitorStatus => {
                let _ = self.projects(tx);
                serde_json::to_value(self.project_monitor.get().and_then(|result|result.as_ref().ok()).map(|manager|manager.status()).unwrap_or_default())?
            }
            Action::ListProjectReadings => {
                let query = serde_json::from_str(&request.json).map_err(|_| io::ErrorKind::InvalidInput)?;
                serde_json::to_value(kanade_indexer::overrides::page(&self.data.join("kanade/projects"), query).map_err(|_| io::ErrorKind::InvalidData)?)?
            }
            Action::PrepareProject => {
                let request = serde_json::from_str(&request.json).map_err(|_| io::ErrorKind::InvalidInput)?;
                serde_json::to_value(self.projects(tx)?.prepare(request).map_err(|_| io::ErrorKind::InvalidData)?)?
            }
            Action::StartProjectJob => {
                let request = serde_json::from_str(&request.json).map_err(|_| io::ErrorKind::InvalidInput)?;
                serde_json::to_value(self.projects(tx)?.submit(request).map_err(|_| io::ErrorKind::InvalidData)?)?
            }
            Action::ProjectJobStatus | Action::CancelProjectJob => {
                match self.project_jobs.get() {
                    Some(Ok(service)) if action == Action::CancelProjectJob => serde_json::to_value(service.cancel(request.job_id).map_err(|_| io::ErrorKind::InvalidData)?)?,
                    Some(Ok(service)) => serde_json::to_value(service.status(request.job_id).map_err(|_| io::ErrorKind::InvalidData)?)?,
                    _ if action == Action::ProjectJobStatus && request.job_id == 0 => serde_json::json!([]),
                    _ => return Err(io::ErrorKind::NotFound.into()),
                }
            }
            Action::ListProjects => {
                let registry = kanade_indexer::Registry::load(&self.data.join("kanade/projects"))
                    .map_err(|_| io::ErrorKind::InvalidData)?;
                if request.json.trim().is_empty() || request.json.trim() == "null" {
                    project_summary(registry)
                } else {
                    let query = serde_json::from_str(&request.json)
                        .map_err(|_| io::ErrorKind::InvalidInput)?;
                    serde_json::to_value(kanade_indexer::management::app_scope(&registry, &query)
                        .map_err(|_| io::ErrorKind::InvalidData)?)?
                }
            }
            Action::EditProject => {
                use kanade_indexer::management::{EditError, EditState, Listing, Reply};
                let edit = serde_json::from_str(&request.json).map_err(|_| io::ErrorKind::InvalidInput)?;
                let data = self.data.join("kanade/projects");
                let reply = match kanade_indexer::management::apply(&data, edit) {
                    Ok(outcome) => Reply { result: EditState::Applied, listing: Some(outcome.listing), cleanup_complete: outcome.cleanup_complete, reloaded: self.reload_projects(tx).is_ok() },
                    Err(error) => Reply {
                        result: match error { EditError::Stale => EditState::Stale, EditError::NotFound => EditState::NotFound, EditError::Busy => EditState::Busy, EditError::Invalid => EditState::Invalid, EditError::Unavailable => EditState::Unavailable },
                        listing: kanade_indexer::Registry::load(&data).ok().map(|r| Listing::from(&r)),
                        cleanup_complete: false, reloaded: false,
                    },
                };
                serde_json::to_value(reply)?
            }
            Action::ReloadProjects => {
                self.reload_projects(tx)?;
                serde_json::json!({"reloaded":true})
            }
            Action::GetSettings => {
                let mut current = self.settings.lock().map_err(|_| io::ErrorKind::Other)?;
                // The file may have been repaired or removed since. Reading it
                // again writes nothing; settings that can be read take effect.
                // Settings the engine could not be handed are retried by the
                // next request; until then they are still reported unreadable.
                if current.unreadable
                    && let Ok(settings) = config::Config::load(&self.settings_file())
                {
                    let _ = self.adopt(&mut current, settings, tx);
                }
                settings_state = Some(current.state());
                serde_json::to_value(&current.config)?
            }
            Action::SetSettings => {
                let edit: SettingsRequest = serde_json::from_str(&request.json)
                    .map_err(|_| io::ErrorKind::InvalidInput)?;
                let mut current = self.settings.lock().map_err(|_| io::ErrorKind::Other)?;
                let settings = match edit {
                    SettingsRequest::Replace(settings) => settings,
                    SettingsRequest::Edit(edit) => {
                        let serde_json::Value::Object(mut merged) = serde_json::to_value(&current.config)? else {
                            return Err(io::ErrorKind::InvalidData.into());
                        };
                        merged.extend(edit.changes);
                        serde_json::from_value(serde_json::Value::Object(merged))
                            .map_err(|_| io::ErrorKind::InvalidInput)?
                    }
                };
                settings.validate()?;
                // Switching the model on needs a qualified model and runtime.
                // Other settings stay editable while `llm` is already on.
                if settings.llm && !current.config.llm && !self.llm.ready() {
                    return Err(io::ErrorKind::Unsupported.into());
                }
                self.apply_settings(&mut current, settings, tx)?;
                serde_json::json!({"saved":true})
            }
            #[allow(deprecated)]
            Action::SetLearningApps => return Err(io::ErrorKind::Unsupported.into()),
            Action::SetLearningExclusions => {
                let edit: config::LearningExclusionsEdit = serde_json::from_str(&request.json)
                    .map_err(|_| io::ErrorKind::InvalidInput)?;
                let mut current = self.settings.lock().map_err(|_| io::ErrorKind::Other)?;
                let settings = edit.apply(&current.config)?;
                self.apply_settings(&mut current, settings, tx)?;
                serde_json::to_value(&current.config)?
            }
            Action::LlmStatus => serde_json::to_value(self.llm.status())?,
            Action::LlmImport => {
                let request = serde_json::from_str(&request.json)
                    .map_err(|_| io::ErrorKind::InvalidInput)?;
                let started = match request {
                    ModelImport::Import(import) => self.llm.import(import),
                    ModelImport::Activate(request) => self.start_activating(tx, |activation| {
                        self.llm.import_with_activation(request.import_and_activate, activation)
                    })?,
                };
                serde_json::to_value(started)?
            }
            Action::LlmQualify => {
                let options: ModelQualification = if request.json.is_empty() {
                    ModelQualification::default()
                } else {
                    serde_json::from_str(&request.json).map_err(|_| io::ErrorKind::InvalidInput)?
                };
                serde_json::to_value(if options.activate {
                    self.start_activating(tx, |activation| self.llm.qualify_with_activation(activation))?
                } else {
                    self.llm.qualify()
                })?
            }
            Action::LlmRemove => {
                let mut current = self.settings.lock().map_err(|_| io::ErrorKind::Other)?;
                let result = self.llm.remove();
                if result != kanade_llm::manager::RemoveResult::Busy && current.config.llm {
                    let mut settings = current.config.clone();
                    settings.llm = false;
                    self.apply_settings(&mut current, settings, tx)?;
                }
                serde_json::json!({"result": result})
            }
            Action::ListLearning => self.learning.as_ref().and_then(|learning| learning.read(|store| {
                serde_json::json!({
                    "records":store.records().skip(request.offset as usize).take(50).cloned().collect::<Vec<_>>(),
                    "total":store.records().count(), "offset":request.offset
                })
            })).unwrap_or_else(|| serde_json::json!({"records":[],"total":0,"offset":0})),
            Action::ListMozcDictionary => serde_json::to_value(
                kanade_mozc::dictionary::read(&self.data.join("mozc"), request.offset)
                    .map_err(|_| io::ErrorKind::InvalidData)?
            )?,
            Action::EditMozcDictionary => {
                let edit = serde_json::from_str(&request.json)
                    .map_err(|_| io::ErrorKind::InvalidInput)?;
                serde_json::to_value(self.dictionary(tx)?.submit(edit)?)?
            }
            Action::DictionaryJobStatus | Action::CancelDictionaryJob => {
                match self.dictionary.get() {
                    Some(Ok(service)) => {
                        if action == Action::CancelDictionaryJob {
                            serde_json::to_value(service.cancel(request.job_id)?)?
                        } else {
                            serde_json::to_value(service.status(request.job_id)?)?
                        }
                    }
                    _ if action == Action::DictionaryJobStatus && request.job_id == 0 => {
                        serde_json::json!([])
                    }
                    _ => return Err(io::ErrorKind::NotFound.into()),
                }
            }
            Action::PromoteLearning => {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Promotion {
                    revision: String,
                    dictionary_index: Option<u32>,
                    #[serde(default)]
                    pos: Option<String>,
                }
                let proposal: Promotion = serde_json::from_str(&request.json)
                    .map_err(|_| io::ErrorKind::InvalidInput)?;
                let correction = self.learner()?.read(|store| {
                    store.records().find(|r| r.id == request.record_id)
                        .map(|r| r.correction.clone())
                }).flatten().ok_or(io::ErrorKind::NotFound)?;
                let edit = kanade_mozc::dictionary::Edit::Upsert {
                    revision: proposal.revision,
                    dictionary_index: proposal.dictionary_index,
                    entry_index: None,
                    entry: kanade_mozc::dictionary::NewEntry {
                        reading: correction.reading,
                        value: correction.accepted,
                        pos: proposal.pos.unwrap_or_else(|| "noun".into()),
                        comment: String::new(),
                    },
                };
                serde_json::to_value(self.dictionary(tx)?.submit(edit)?)?
            }
            Action::DeleteLearning => {
                self.learner()?.manage(AdminAction::Delete(request.record_id))?;
                serde_json::json!({"deleted":true})
            }
            Action::ClearLearning => {
                self.learner()?.manage(AdminAction::Clear)?;
                serde_json::json!({"cleared":true})
            }
            Action::BlockReading => {
                let reading: String = serde_json::from_str(&request.json)
                    .map_err(|_| io::ErrorKind::InvalidInput)?;
                if reading.len() > 256 {
                    return Err(io::ErrorKind::InvalidInput.into());
                }
                self.learner()?.manage(AdminAction::Block(reading))?;
                serde_json::json!({"blocked":true})
            }
            Action::ClearMozcConversion | Action::ClearMozcPrediction => {
                let kind = if action == Action::ClearMozcConversion {
                    kanade_mozc::LearningKind::Conversion
                } else {
                    kanade_mozc::LearningKind::Prediction
                };
                let (ack, done) = mpsc::sync_channel(1);
                tx.try_send(Work::ClearMozcLearning(
                    kind, Instant::now() + Duration::from_secs(4), ack
                )).map_err(|_| io::ErrorKind::WouldBlock)?;
                match done.recv_timeout(Duration::from_secs(5))
                    .map_err(|_| io::ErrorKind::TimedOut)? {
                    Ok(()) => serde_json::json!({"cleared":true}),
                    Err(failure::Code::Sequence) => serde_json::json!({"cleared":false,"reason":"composition_in_progress"}),
                    Err(_) => serde_json::json!({"cleared":false,"reason":"worker_unavailable"}),
                }
            }
        };
        let (settings, learning) = self.stores();
        let settings = settings_state.unwrap_or(settings);
        // The settings application may run with another data directory, so
        // it shows the file the engine opened rather than one of its own.
        let named = |state, file: PathBuf| {
            if state == StoreState::Unreadable {
                file.to_string_lossy().into_owned()
            } else {
                String::new()
            }
        };
        Ok(Management {
            json: serde_json::to_string(&value)?,
            settings: settings as i32,
            learning: learning as i32,
            settings_file: named(settings, self.settings_file()),
            learning_file: named(learning, self.learning_file()),
        })
    }
    fn start_activating(
        self: &Arc<Self>,
        tx: &SyncSender<Work>,
        start: impl FnOnce(Option<Activation>) -> Started,
    ) -> io::Result<Started> {
        // Take the job slot before another settings operation can turn the
        // previous model off or remove it.
        let current = self.settings.lock().map_err(|_| io::ErrorKind::Other)?;
        if current.unreadable {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        if current.config.llm {
            return Ok(start(None));
        }
        let admin = Arc::downgrade(self);
        let tx = tx.clone();
        Ok(start(Some(Box::new(move || {
            let admin = admin.upgrade().ok_or(io::ErrorKind::BrokenPipe)?;
            let mut current = admin.settings.lock().map_err(|_| io::ErrorKind::Other)?;
            if !current.config.llm {
                let mut settings = current.config.clone();
                settings.llm = true;
                admin.apply_settings(&mut current, settings, &tx)?;
            }
            Ok(())
        }))))
    }
    pub(super) fn reload_projects(&self, tx: &SyncSender<Work>) -> io::Result<bool> {
        reload_catalog(&self.data, &self.project_reload, tx)
    }
    pub(super) fn start_projects(&self, tx: &SyncSender<Work>) {
        let _ = self.projects(tx);
    }
    fn projects(
        &self,
        tx: &SyncSender<Work>,
    ) -> io::Result<&Arc<kanade_indexer::workflow::Service>> {
        self.project_jobs
            .get_or_init(|| {
                let data = self.data.clone();
                let reload = self.project_reload.clone();
                let engine = tx.clone();
                let service = Arc::new(
                    kanade_indexer::workflow::Service::start(
                        self.data.join("kanade/projects"),
                        kanade_indexer::workflow::analyzer_path()
                            .map_err(|_| io::ErrorKind::NotFound)?,
                        move || reload_catalog(&data, &reload, &engine).is_ok_and(|retry| !retry),
                    )
                    .map_err(|_| io::Error::from(io::ErrorKind::Other))?,
                );
                let _ = self.project_monitor.get_or_init(|| {
                    kanade_indexer::monitor::Manager::start(
                        self.data.join("kanade/projects"),
                        service.clone(),
                    )
                    .map_err(|_| io::ErrorKind::Other.into())
                });
                Ok(service)
            })
            .as_ref()
            .map_err(|_| io::ErrorKind::Other.into())
    }
    /// Whether the settings file and the learning store could be read.
    fn stores(&self) -> (StoreState, StoreState) {
        let settings = self
            .settings
            .lock()
            .map_or(StoreState::Unspecified, |current| current.state());
        let learning = if self.learning.is_some() {
            StoreState::Read
        } else {
            StoreState::Unreadable
        };
        (settings, learning)
    }
    fn settings_file(&self) -> PathBuf {
        super::settings_file(&self.data)
    }
    fn learning_file(&self) -> PathBuf {
        super::learning_file(&self.data)
    }
    /// Saves, applies to the engine and updates the model service, in that
    /// order. A settings file that could not be read is refused rather than
    /// written over.
    fn apply_settings(
        &self,
        current: &mut Settings,
        settings: config::Config,
        tx: &SyncSender<Work>,
    ) -> io::Result<()> {
        if current.unreadable {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        settings.save(&self.settings_file())?;
        self.adopt(current, settings, tx)
    }
    /// Applies settings to the engine and the model service without saving.
    /// Settings handed to the engine are the ones in force from then on: it
    /// applies its work in order, even after the wait for its answer ends, so
    /// a late answer leaves no report of the settings it replaced.
    fn adopt(
        &self,
        current: &mut Settings,
        settings: config::Config,
        tx: &SyncSender<Work>,
    ) -> io::Result<()> {
        let (ack, done) = mpsc::sync_channel(1);
        tx.send(Work::Configure(settings.clone(), ack))
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        self.llm.set_enabled(settings.llm);
        *current = Settings {
            config: settings,
            unreadable: false,
        };
        done.recv_timeout(Duration::from_secs(5))
            .map_err(|_| io::ErrorKind::TimedOut)?;
        Ok(())
    }
    fn learner(&self) -> io::Result<&LearningService> {
        self.learning
            .as_ref()
            .ok_or(io::ErrorKind::Unsupported.into())
    }
    fn dictionary(&self, tx: &SyncSender<Work>) -> io::Result<&dictionary::Service> {
        self.dictionary
            .get_or_init(|| dictionary::Service::start(self.data.join("mozc"), tx.clone()))
            .as_ref()
            .map_err(|_| io::ErrorKind::Other.into())
    }
}

fn reload_catalog(
    data: &std::path::Path,
    reload: &Mutex<Arc<kanade_indexer::Catalog>>,
    tx: &SyncSender<Work>,
) -> io::Result<bool> {
    // Serialize explicit and notification reloads through publication, so an older
    // disk snapshot cannot overtake a newer one in the engine queue.
    let mut current = reload.lock().map_err(|_| io::ErrorKind::Other)?;
    let catalog = kanade_indexer::Catalog::refresh(&data.join("kanade/projects"), &current);
    let invalid = catalog.is_err();
    let (catalog, retry) = catalog.unwrap_or_default();
    let catalog = Arc::new(catalog);
    let (ack, done) = mpsc::sync_channel(1);
    tx.send(Work::Projects(catalog.clone(), ack))
        .map_err(|_| io::ErrorKind::BrokenPipe)?;
    done.recv_timeout(Duration::from_secs(5))
        .map_err(|_| io::ErrorKind::TimedOut)?;
    // Retain the old Arc until publication, then release it off the key thread.
    *current = catalog;
    // Unreadable approvals fail closed instead of retaining revoked vocabulary.
    if invalid {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(retry)
}

fn project_summary(registry: kanade_indexer::Registry) -> serde_json::Value {
    // Management lists metadata only; large reading overrides stay on disk.
    serde_json::json!(kanade_indexer::management::Listing::from(&registry))
}
