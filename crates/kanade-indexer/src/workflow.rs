//! Explicit project approval and full-index jobs. Status retains metadata only.
use crate::{
    Project, Registry, directory_binding,
    management::{ScopeQuery, valid_apps},
    scan, valid_id,
};
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) struct Control(AtomicU8);
impl Control {
    pub(crate) fn new() -> Self {
        Self(AtomicU8::new(0))
    }
    pub(crate) fn cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire) == 2
    }
    pub(crate) fn cancel(&self) -> bool {
        matches!(
            self.0
                .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire),
            Ok(_) | Err(2)
        )
    }
    pub(crate) fn publish(&self) -> Result<()> {
        self.0
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| anyhow::anyhow!("index cancelled"))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequest {
    pub revision: u64,
    pub id: String,
    pub root: PathBuf,
    pub apps: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preview {
    pub token: String,
    pub revision: u64,
    pub id: String,
    pub root: PathBuf,
    pub apps: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Approve { token: String },
    Index { query: ScopeQuery },
    Reading { edit: crate::overrides::Edit },
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Queued,
    Preparing,
    Indexing,
    Reloading,
    Ready,
    Failed,
    Cancelled,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub id: String,
    pub state: Stage,
    pub saved_revision: Option<String>,
    pub terms: Option<usize>,
    pub reloaded: bool,
    pub reason: Option<String>,
}
impl Status {
    pub fn finished(&self) -> bool {
        matches!(self.state, Stage::Ready | Stage::Failed | Stage::Cancelled)
    }
}
struct Plan {
    preview: Preview,
    binding: String,
    expires: Instant,
}
struct Slot {
    status: Status,
    control: Arc<Control>,
    background: bool,
}
struct State {
    next: u64,
    plans: BTreeMap<u64, Plan>,
    jobs: BTreeMap<u64, Slot>,
}
enum Input {
    Approve(Plan),
    Index(Project),
    Reading(crate::overrides::Edit),
}
struct Job {
    id: u64,
    input: Input,
    control: Arc<Control>,
}
pub struct Service {
    data: PathBuf,
    state: Arc<Mutex<State>>,
    tx: mpsc::SyncSender<Job>,
}

fn identity() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}
fn registry_project(data: &Path, query: &ScopeQuery) -> Result<Project> {
    let registry = Registry::load(data)?;
    ensure!(
        registry.revision == query.revision && valid_id(&query.id),
        "project changed"
    );
    registry
        .projects
        .into_iter()
        .find(|project| {
            project.id == query.id && project.approval == query.approval && project.enabled
        })
        .ok_or_else(|| anyhow::anyhow!("project changed or disabled"))
}
impl Service {
    pub fn start(
        data: PathBuf,
        analyzer: PathBuf,
        notify: impl Fn() -> bool + Send + 'static,
    ) -> Result<Self> {
        drop(crate::storage_lock(&data, "registry")?);
        let state = Arc::new(Mutex::new(State {
            next: identity().max(1),
            plans: BTreeMap::new(),
            jobs: BTreeMap::new(),
        }));
        let shared = state.clone();
        let folder = data.clone();
        let (tx, rx) = mpsc::sync_channel::<Job>(2);
        std::thread::Builder::new()
            .name("kanade-project-jobs".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    run(job, &folder, &analyzer, &shared, &notify);
                }
            })?;
        Ok(Self { data, state, tx })
    }
    pub fn prepare(&self, mut request: ApprovalRequest) -> Result<Preview> {
        ensure!(
            valid_id(&request.id) && valid_apps(&request.apps),
            "invalid approval"
        );
        let registry = Registry::load(&self.data)?;
        ensure!(
            registry.revision == request.revision
                && registry.projects.len() < 32
                && !registry
                    .projects
                    .iter()
                    .any(|project| project.id == request.id),
            "approval changed"
        );
        request.root = request.root.canonicalize()?;
        let data = self.data.canonicalize()?;
        ensure!(
            !request.root.starts_with(&data) && !data.starts_with(&request.root),
            "source overlaps storage"
        );
        let binding = directory_binding(&request.root)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("project state unavailable"))?;
        state.plans.retain(|_, plan| plan.expires > Instant::now());
        // Repeated review must not lock the current window out. Evicting an
        // older preview only makes its later submission fail closed.
        while state.plans.len() >= 4 {
            state.plans.pop_first();
        }
        state.next = state
            .next
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("project identity exhausted"))?;
        let id = state.next;
        let preview = Preview {
            token: id.to_string(),
            revision: request.revision,
            id: request.id,
            root: request.root,
            apps: request.apps,
        };
        state.plans.insert(
            id,
            Plan {
                preview: preview.clone(),
                binding,
                expires: Instant::now() + Duration::from_secs(300),
            },
        );
        Ok(preview)
    }
    pub fn submit(&self, request: Request) -> Result<Status> {
        self.submit_inner(request, false)
    }
    pub(crate) fn submit_background(&self, query: ScopeQuery) -> Result<Status> {
        self.submit_inner(Request::Index { query }, true)
    }
    fn submit_inner(&self, request: Request, background: bool) -> Result<Status> {
        let indexed = match &request {
            Request::Index { query } => Some(registry_project(&self.data, query)?),
            _ => None,
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("project state unavailable"))?;
        ensure!(
            !state
                .jobs
                .values()
                .any(|slot| !slot.status.finished() && (background || !slot.background)),
            "project job already running"
        );
        let (id, input) = match request {
            Request::Approve { token } => {
                let id = token.parse::<u64>()?;
                let plan = state
                    .plans
                    .remove(&id)
                    .ok_or_else(|| anyhow::anyhow!("approval preview unavailable"))?;
                ensure!(plan.expires > Instant::now(), "approval preview expired");
                (id, Input::Approve(plan))
            }
            Request::Index { .. } => {
                state.next = state
                    .next
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("project identity exhausted"))?;
                (
                    state.next,
                    Input::Index(indexed.expect("checked index request")),
                )
            }
            Request::Reading { edit } => {
                ensure!(edit.valid(), "invalid reading edit");
                registry_project(&self.data, &edit.query)?;
                state.next = state
                    .next
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("project identity exhausted"))?;
                (state.next, Input::Reading(edit))
            }
        };
        if !background {
            for slot in state
                .jobs
                .values()
                .filter(|slot| slot.background && !slot.status.finished())
            {
                let _ = slot.control.cancel();
            }
        }
        let status = Status {
            id: id.to_string(),
            state: Stage::Queued,
            saved_revision: None,
            terms: None,
            reloaded: false,
            reason: None,
        };
        let control = Arc::new(Control::new());
        self.tx
            .try_send(Job {
                id,
                input,
                control: control.clone(),
            })
            .map_err(|_| anyhow::anyhow!("project service unavailable"))?;
        state.jobs.insert(
            id,
            Slot {
                status: status.clone(),
                control,
                background,
            },
        );
        while state.jobs.len() > 16 {
            state.jobs.pop_first();
        }
        Ok(status)
    }
    pub fn status(&self, id: u64) -> Result<Vec<Status>> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("project state unavailable"))?;
        if id == 0 {
            Ok(state
                .jobs
                .values()
                .map(|slot| slot.status.clone())
                .collect())
        } else {
            Ok(vec![
                state
                    .jobs
                    .get(&id)
                    .ok_or_else(|| anyhow::anyhow!("project job unavailable"))?
                    .status
                    .clone(),
            ])
        }
    }
    pub fn cancel(&self, id: u64) -> Result<Status> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("project state unavailable"))?;
        let slot = state
            .jobs
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("project job unavailable"))?;
        if !slot.status.finished() {
            ensure!(slot.control.cancel(), "index publication already started");
            slot.status.reason = Some("cancellation_requested".into());
        }
        Ok(slot.status.clone())
    }
}
fn update(state: &Mutex<State>, id: u64, change: impl FnOnce(&mut Status)) {
    if let Ok(mut state) = state.lock()
        && let Some(slot) = state.jobs.get_mut(&id)
    {
        change(&mut slot.status);
    }
}
fn approve(data: &Path, plan: Plan) -> Result<(Project, u64)> {
    let mut registry = Registry::load(data)?;
    let preview = plan.preview;
    ensure!(
        registry.revision == preview.revision
            && registry.projects.len() < 32
            && !registry
                .projects
                .iter()
                .any(|project| project.id == preview.id),
        "approval changed"
    );
    ensure!(
        directory_binding(&preview.root)? == plan.binding,
        "approved directory changed"
    );
    let approval = identity().max(
        registry
            .projects
            .iter()
            .map(|project| project.approval)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("approval identity exhausted"))?,
    );
    let project = Project {
        id: preview.id,
        approval,
        root: preview.root,
        monitoring: false,
        directory_binding: Some(plan.binding),
        apps: preview.apps,
        enabled: true,
        readings: Default::default(),
    };
    registry.projects.push(project.clone());
    registry.save(data)?;
    Ok((project, registry.revision))
}
fn run(job: Job, data: &Path, analyzer: &Path, state: &Mutex<State>, notify: &impl Fn() -> bool) {
    update(state, job.id, |status| status.state = Stage::Preparing);
    let outcome = (|| -> Result<usize> {
        ensure!(!job.control.cancelled(), "index cancelled");
        let project = match job.input {
            Input::Approve(plan) => {
                let (project, revision) = approve(data, plan)?;
                update(state, job.id, |status| {
                    status.saved_revision = Some(revision.to_string())
                });
                project
            }
            Input::Index(project) => project,
            Input::Reading(edit) => {
                let (project, revision) = crate::overrides::apply(data, edit)?;
                update(state, job.id, |status| {
                    status.saved_revision = Some(revision.to_string())
                });
                project
            }
        };
        ensure!(!job.control.cancelled(), "index cancelled");
        update(state, job.id, |status| status.state = Stage::Indexing);
        scan::index_bound(data, &project, analyzer, &job.control)
    })();
    match outcome {
        Ok(terms) => {
            update(state, job.id, |status| {
                status.state = Stage::Reloading;
                status.terms = Some(terms);
            });
            let reloaded = notify();
            update(state, job.id, |status| {
                status.state = Stage::Ready;
                status.reloaded = reloaded;
                status.reason = None;
            });
        }
        Err(_) => update(state, job.id, |status| {
            status.state = if job.control.cancelled() {
                Stage::Cancelled
            } else {
                Stage::Failed
            };
            status.reason = Some(
                if job.control.cancelled() {
                    "cancelled"
                } else if status.saved_revision.is_some() {
                    "index_failed"
                } else {
                    "approval_or_index_failed"
                }
                .into(),
            );
        }),
    }
}

/// Installed engines use their signed sibling. Development may select an analyzer explicitly.
pub fn analyzer_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("KANADE_ASTRO_SIGHT") {
        return Ok(path.into());
    }
    let sibling = std::env::current_exe()?.with_file_name(if cfg!(windows) {
        "kanade-project-analyzer.exe"
    } else {
        "kanade-project-analyzer"
    });
    if sibling.is_file() {
        Ok(sibling)
    } else {
        bail!("bundled project analyzer unavailable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Service, ApprovalRequest) {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let service = Service::start(
            temp.path().join("data"),
            temp.path().join("missing-analyzer"),
            || true,
        )
        .unwrap();
        let request = ApprovalRequest {
            revision: 0,
            id: "sample".into(),
            root: source,
            apps: vec!["com.example.Editor".into()],
        };
        (temp, service, request)
    }
    fn finished(service: &Service, id: &str) -> Status {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = service.status(id.parse().unwrap()).unwrap().remove(0);
            if status.finished() {
                return status;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn preview_does_not_grant_access_and_replaced_directory_cannot_be_approved() {
        let (temp, service, request) = fixture();
        let preview = service.prepare(request.clone()).unwrap();
        assert!(Registry::load(&service.data).unwrap().projects.is_empty());
        std::fs::rename(&request.root, temp.path().join("previous")).unwrap();
        std::fs::create_dir(&request.root).unwrap();
        let status = service
            .submit(Request::Approve {
                token: preview.token,
            })
            .unwrap();
        let status = finished(&service, &status.id);
        assert_eq!(status.state, Stage::Failed);
        assert!(status.saved_revision.is_none());
        assert!(Registry::load(&service.data).unwrap().projects.is_empty());
    }
    #[test]
    fn failed_index_preserves_explicit_approval_and_status_contains_no_vocabulary() {
        let (_temp, service, request) = fixture();
        let preview = service.prepare(request.clone()).unwrap();
        let token = preview.token.clone();
        let status = service
            .submit(Request::Approve {
                token: token.clone(),
            })
            .unwrap();
        let status = finished(&service, &status.id);
        assert_eq!(status.state, Stage::Failed);
        assert!(status.saved_revision.is_some());
        let registry = Registry::load(&service.data).unwrap();
        assert_eq!(registry.projects[0].apps, request.apps);
        assert!(registry.projects[0].directory_binding.is_some());
        assert!(service.submit(Request::Approve { token }).is_err());
        let encoded = serde_json::to_string(&status).unwrap();
        assert!(!encoded.contains("com.example.Editor"));
        assert!(!encoded.contains(request.root.to_str().unwrap()));
    }
    #[test]
    fn preview_count_and_source_storage_overlap_are_bounded() {
        let (_temp, service, mut request) = fixture();
        request.root = service.data.clone();
        assert!(service.prepare(request.clone()).is_err());
        request.root = service.data.parent().unwrap().join("source");
        request.apps.clear();
        assert!(service.prepare(request.clone()).is_err());
        request.apps = vec!["com.example.Editor".into()];
        let first = service.prepare(request.clone()).unwrap();
        for _ in 0..4 {
            service.prepare(request.clone()).unwrap();
        }
        assert!(
            service
                .submit(Request::Approve { token: first.token })
                .is_err()
        );
        assert_eq!(service.state.lock().unwrap().plans.len(), 4);
        assert!(Registry::load(&service.data).unwrap().projects.is_empty());
    }
    #[test]
    fn publication_and_cancellation_are_exclusive() {
        let cancelled = Control::new();
        assert!(cancelled.cancel());
        assert!(cancelled.cancel());
        assert!(cancelled.publish().is_err());
        let published = Control::new();
        published.publish().unwrap();
        assert!(!published.cancel());
    }
    #[test]
    #[ignore = "requires the pinned analyzer; set KANADE_ASTRO_SIGHT"]
    fn manual_reading_edits_queue_behind_a_background_publication() {
        let analyzer = PathBuf::from(std::env::var_os("KANADE_ASTRO_SIGHT").expect("set analyzer"));
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("one.rs"), "pub struct CacheRequest;\n").unwrap();
        let data = temp.path().join("data");
        let mut registry = Registry::default();
        registry.projects.push(Project {
            id: "sample".into(),
            approval: 7,
            root,
            directory_binding: None,
            monitoring: true,
            apps: vec!["com.example.Editor".into()],
            enabled: true,
            readings: Default::default(),
        });
        registry.save(&data).unwrap();
        let (entered, waiting) = mpsc::sync_channel(1);
        let (release, gate) = mpsc::sync_channel(1);
        let gate = Mutex::new(Some(gate));
        let service = Service::start(data.clone(), analyzer, move || {
            if let Some(gate) = gate.lock().unwrap().take() {
                entered.send(()).unwrap();
                let _ = gate.recv();
            }
            true
        })
        .unwrap();
        let query = ScopeQuery {
            revision: registry.revision,
            id: "sample".into(),
            approval: 7,
        };
        let background = service.submit_background(query.clone()).unwrap();
        waiting.recv_timeout(Duration::from_secs(30)).unwrap();
        let manual = service
            .submit(Request::Reading {
                edit: crate::overrides::Edit {
                    query: query.clone(),
                    identifier: "CacheRequest".into(),
                    reading: Some("キャッシュ".into()),
                },
            })
            .unwrap();
        assert_eq!(manual.state, Stage::Queued);
        assert!(service.submit(Request::Index { query }).is_err());
        release.send(()).unwrap();
        assert_eq!(finished(&service, &background.id).state, Stage::Ready);
        assert_eq!(finished(&service, &manual.id).state, Stage::Ready);
        assert_eq!(
            crate::Catalog::load(&data)
                .unwrap()
                .lookup("com.example.Editor", "キャッシュ"),
            ["CacheRequest"]
        );
    }
    #[test]
    #[ignore = "requires the pinned analyzer; set KANADE_ASTRO_SIGHT"]
    fn reading_jobs_rebuild_and_report_saved_edits_after_analysis_failure() {
        let analyzer = PathBuf::from(std::env::var_os("KANADE_ASTRO_SIGHT").expect("set analyzer"));
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("one.rs"), "pub struct CacheRequest;\n").unwrap();
        let data = temp.path().join("data");
        let service = Service::start(data.clone(), analyzer, || true).unwrap();
        let preview = service
            .prepare(ApprovalRequest {
                revision: 0,
                id: "sample".into(),
                root,
                apps: vec!["com.example.Editor".into()],
            })
            .unwrap();
        let first = service
            .submit(Request::Approve {
                token: preview.token,
            })
            .unwrap();
        assert_eq!(finished(&service, &first.id).state, Stage::Ready);
        let registry = Registry::load(&data).unwrap();
        let edit = crate::overrides::Edit {
            query: ScopeQuery {
                revision: registry.revision,
                id: "sample".into(),
                approval: registry.projects[0].approval,
            },
            identifier: "CacheRequest".into(),
            reading: Some("キャッシュ".into()),
        };
        let job = service
            .submit(Request::Reading { edit: edit.clone() })
            .unwrap();
        let done = finished(&service, &job.id);
        assert_eq!(done.state, Stage::Ready);
        assert!(done.saved_revision.is_some());
        let catalog = crate::Catalog::load(&data).unwrap();
        assert_eq!(
            catalog.lookup("com.example.Editor", "キャッシュ"),
            ["CacheRequest"]
        );
        assert!(
            catalog
                .lookup("com.example.Editor", "キャッシュリクエスト")
                .is_empty()
        );
        assert!(
            service
                .submit(Request::Reading { edit: edit.clone() })
                .is_err()
        );
        let mut reset = edit.clone();
        reset.query.revision = Registry::load(&data).unwrap().revision;
        reset.reading = None;
        let job = service.submit(Request::Reading { edit: reset }).unwrap();
        assert_eq!(finished(&service, &job.id).state, Stage::Ready);
        assert_eq!(
            crate::Catalog::load(&data)
                .unwrap()
                .lookup("com.example.Editor", "キャッシュリクエスト"),
            ["CacheRequest"]
        );
        let pointer = data.join("sample/current.json");
        let before = std::fs::read(&pointer).unwrap();
        let broken = Service::start(data.clone(), temp.path().join("missing"), || true).unwrap();
        let mut changed = edit;
        changed.query.revision = Registry::load(&data).unwrap().revision;
        let job = broken.submit(Request::Reading { edit: changed }).unwrap();
        let done = finished(&broken, &job.id);
        assert_eq!(done.state, Stage::Failed);
        assert!(done.saved_revision.is_some());
        assert_eq!(std::fs::read(pointer).unwrap(), before);
        assert_eq!(
            Registry::load(&data).unwrap().projects[0].readings["CacheRequest"],
            "キャッシュ"
        );
        let status = serde_json::to_string(&done).unwrap();
        assert!(!status.contains("CacheRequest"));
        assert!(!status.contains("キャッシュ"));
    }
    #[test]
    #[ignore = "requires the pinned analyzer; set KANADE_ASTRO_SIGHT"]
    fn approval_and_full_rebuild_use_bound_snapshots_and_keep_a_failed_generation() {
        let analyzer = PathBuf::from(std::env::var_os("KANADE_ASTRO_SIGHT").expect("set analyzer"));
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("one.rs"), "pub struct CacheRequest;\n").unwrap();
        let data = temp.path().join("data");
        let service = Service::start(data.clone(), analyzer.clone(), || true).unwrap();
        let preview = service
            .prepare(ApprovalRequest {
                revision: 0,
                id: "sample".into(),
                root: source.clone(),
                apps: vec!["com.example.Editor".into()],
            })
            .unwrap();
        let first = service
            .submit(Request::Approve {
                token: preview.token,
            })
            .unwrap();
        let first = finished(&service, &first.id);
        assert_eq!(first.state, Stage::Ready);
        assert_eq!(first.terms, Some(1));
        assert!(first.reloaded);
        let catalog = crate::Catalog::load(&data).unwrap();
        assert_eq!(
            catalog.lookup("com.example.Editor", "キャッシュリクエスト"),
            ["CacheRequest"]
        );
        assert!(
            catalog
                .lookup("com.example.Other", "キャッシュリクエスト")
                .is_empty()
        );
        std::fs::write(source.join("two.rs"), "pub struct JsonParser;\n").unwrap();
        let registry = Registry::load(&data).unwrap();
        let project = registry.projects[0].clone();
        let query = ScopeQuery {
            revision: registry.revision,
            id: project.id.clone(),
            approval: project.approval,
        };
        let next = service
            .submit(Request::Index {
                query: query.clone(),
            })
            .unwrap();
        assert_eq!(finished(&service, &next.id).terms, Some(2));
        let pointer = data.join("sample/current.json");
        let before = std::fs::read(&pointer).unwrap();
        let broken = Service::start(data.clone(), temp.path().join("missing"), || true).unwrap();
        let failed = broken.submit(Request::Index { query }).unwrap();
        assert_eq!(finished(&broken, &failed.id).state, Stage::Failed);
        assert_eq!(std::fs::read(&pointer).unwrap(), before);
        let cancelled = Control::new();
        assert!(cancelled.cancel());
        assert!(scan::index_bound(&data, &project, &analyzer, &cancelled).is_err());
        assert_eq!(std::fs::read(&pointer).unwrap(), before);
        let previous = crate::Catalog::load(&data).unwrap();
        std::fs::rename(&source, temp.path().join("previous")).unwrap();
        std::fs::create_dir(&source).unwrap();
        assert!(scan::index_bound(&data, &project, &analyzer, &Control::new()).is_err());
        let (catalog, _) = crate::Catalog::refresh(&data, &previous).unwrap();
        assert!(
            catalog
                .lookup("com.example.Editor", "キャッシュリクエスト")
                .is_empty()
        );
    }
}
