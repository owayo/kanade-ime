//! Opt-in source notifications submit bounded jobs to the shared index queue.
//! Events retain project identities only; paths and vocabulary are never logged.
use crate::{
    Project, Registry,
    management::ScopeQuery,
    workflow::{Service, Stage},
};
use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Watching,
    Waiting,
    Indexing,
    Backoff,
    Unavailable,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Status {
    pub id: String,
    pub state: State,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Report {
    pub supported: bool,
    pub healthy: bool,
    pub projects: Vec<Status>,
}
pub struct Manager {
    stop: Arc<AtomicBool>,
    report: Arc<Mutex<Report>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Manager {
    pub fn start(data: PathBuf, service: Arc<Service>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let report = Arc::new(Mutex::new(Report {
            supported: !cfg!(windows),
            healthy: !cfg!(windows),
            projects: vec![],
        }));
        let thread = if cfg!(windows) {
            None
        } else {
            let shared = report.clone();
            let stopped = stop.clone();
            Some(
                std::thread::Builder::new()
                    .name("kanade-project-monitor".into())
                    .spawn(move || run(data, service, stopped, shared))?,
            )
        };
        Ok(Self {
            stop,
            report,
            thread,
        })
    }
    pub fn status(&self) -> Report {
        self.report.lock().map(|v| v.clone()).unwrap_or_default()
    }
}
impl Drop for Manager {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
#[derive(Default)]
struct Wake {
    registry: bool,
    rebuild: bool,
    projects: BTreeSet<String>,
}
struct Track {
    project: Project,
    state: State,
    dirty: Option<Instant>,
    quiet: Instant,
    not_before: Instant,
    backoff: Duration,
    active: Option<(u64, Instant)>,
}
impl Track {
    fn new(project: Project) -> Self {
        let now = Instant::now();
        Self {
            project,
            state: State::Waiting,
            dirty: Some(now),
            quiet: now + Duration::from_millis(500),
            not_before: now,
            backoff: Duration::from_secs(5),
            active: None,
        }
    }
    fn changed(&mut self) {
        let now = Instant::now();
        self.dirty.get_or_insert(now);
        self.quiet = now + Duration::from_millis(500);
    }
    fn due(&self, now: Instant) -> bool {
        self.dirty.is_some_and(|since| {
            now >= self
                .not_before
                .max(self.quiet.min(since + Duration::from_secs(2)))
        })
    }
}
fn relevant(root: &Path, event: &Event) -> bool {
    event.paths.is_empty() || event.paths.iter().any(|path| {
        root.starts_with(path) || path.strip_prefix(root).is_ok_and(|relative| {
            !relative.components().any(|part|matches!(part,Component::Normal(n) if matches!(n.to_str(),Some(".git"|".hg"|".svn"))))
                || relative==Path::new(".git/info/exclude")
        })
    })
}
fn observe(
    data: &Path,
    roots: &[(String, PathBuf)],
    wake: Arc<Mutex<Wake>>,
    sender: mpsc::SyncSender<()>,
) -> (Option<RecommendedWatcher>, BTreeSet<String>) {
    let source = roots.to_vec();
    let registry = data.join("registry.json");
    let callback = move |event: notify::Result<Event>| {
        let Ok(mut flags) = wake.lock() else {
            return;
        };
        match event {
            Ok(event) if !matches!(event.kind, EventKind::Access(_)) => {
                let all = event.need_rescan();
                flags.rebuild |= all;
                flags.registry |= all
                    || event
                        .paths
                        .iter()
                        .any(|p| p == &registry || registry.starts_with(p));
                for (id, root) in &source {
                    if all || relevant(root, &event) {
                        flags.projects.insert(id.clone());
                    }
                }
            }
            Err(_) => {
                flags.registry = true;
                flags.rebuild = true;
                flags
                    .projects
                    .extend(source.iter().map(|(id, _)| id.clone()));
            }
            _ => return,
        }
        let _ = sender.try_send(());
    };
    let mut failed = BTreeSet::new();
    let Ok(mut watcher) = RecommendedWatcher::new(
        callback,
        notify::Config::default().with_follow_symlinks(false),
    ) else {
        return (None, roots.iter().map(|(id, _)| id.clone()).collect());
    };
    let _ = watcher.watch(data, RecursiveMode::NonRecursive);
    for (id, root) in roots {
        if watcher.watch(root, RecursiveMode::Recursive).is_err() {
            failed.insert(id.clone());
        }
    }
    (Some(watcher), failed)
}
fn cancel(track: &Track, service: &Service) {
    if let Some((id, _)) = track.active {
        let _ = service.cancel(id);
    }
}
fn bound(data: &Path, project: &Project) -> bool {
    project
        .root
        .canonicalize()
        .is_ok_and(|p| p == project.root && !p.starts_with(data) && !data.starts_with(&p))
        && project.directory_binding.as_ref().is_some_and(|expected| {
            crate::directory_binding(&project.root).as_ref().ok() == Some(expected)
        })
}
fn next_project(tracks: &BTreeMap<String, Track>, now: Instant) -> Option<String> {
    tracks
        .iter()
        .filter(|(_, track)| {
            track.state != State::Unavailable && track.active.is_none() && track.due(now)
        })
        .min_by_key(|(_, track)| track.dirty)
        .map(|(id, _)| id.clone())
}
fn run(data: PathBuf, service: Arc<Service>, stop: Arc<AtomicBool>, report: Arc<Mutex<Report>>) {
    let (sender, receiver) = mpsc::sync_channel(1);
    let wake = Arc::new(Mutex::new(Wake::default()));
    let mut tracks: BTreeMap<String, Track> = BTreeMap::new();
    let mut previous: Option<Registry> = None;
    let mut roots = vec![];
    let mut observer = None;
    let mut failed = BTreeSet::new();
    let mut retry_observer = Instant::now();
    let mut next_registry = Instant::now();
    let data = data.canonicalize().unwrap_or(data);
    while !stop.load(Ordering::Acquire) {
        let flags = wake
            .lock()
            .map(|mut flags| std::mem::take(&mut *flags))
            .unwrap_or_default();
        let now = Instant::now();
        if flags.registry || now >= next_registry {
            next_registry = now + Duration::from_secs(2);
            match Registry::load(&data) {
                Ok(registry) => {
                    tracks.retain(|_, track| {
                        let keep = registry
                            .projects
                            .iter()
                            .any(|p| p == &track.project && p.enabled && p.monitoring);
                        if !keep {
                            cancel(track, &service);
                        }
                        keep
                    });
                    for project in registry
                        .projects
                        .iter()
                        .filter(|p| p.enabled && p.monitoring)
                    {
                        tracks
                            .entry(project.id.clone())
                            .or_insert_with(|| Track::new(project.clone()));
                    }
                    let current: Vec<_> = tracks
                        .iter()
                        .filter(|(_, t)| bound(&data, &t.project))
                        .map(|(id, t)| (id.clone(), t.project.root.clone()))
                        .collect();
                    if flags.rebuild
                        || current != roots
                        || previous.as_ref() != Some(&registry)
                        || (now >= retry_observer && (observer.is_none() || !failed.is_empty()))
                    {
                        // Attach before marking scans dirty; changes during indexing stay pending.
                        let (new, errors) = observe(&data, &current, wake.clone(), sender.clone());
                        observer = new;
                        failed = errors;
                        roots = current;
                        retry_observer = now + Duration::from_secs(5);
                        for track in tracks.values_mut() {
                            track.changed();
                        }
                    }
                    for track in tracks.values_mut() {
                        if !roots.iter().any(|(id, _)| id == &track.project.id)
                            || failed.contains(&track.project.id)
                        {
                            cancel(track, &service);
                            track.state = State::Unavailable;
                        } else if track.state == State::Unavailable {
                            track.state = State::Waiting;
                            track.changed();
                        }
                    }
                    previous = Some(registry);
                }
                Err(_) => {
                    for track in tracks.values() {
                        cancel(track, &service);
                    }
                    tracks.clear();
                    observer = None;
                    roots.clear();
                    previous = None;
                    if let Ok(mut status) = report.lock() {
                        status.healthy = false;
                        status.projects.clear();
                    }
                    let _ = receiver.recv_timeout(Duration::from_millis(250));
                    continue;
                }
            }
        }
        for id in flags.projects {
            if let Some(track) = tracks.get_mut(&id) {
                track.changed();
            }
        }
        for track in tracks.values_mut() {
            if track.state == State::Unavailable {
                continue;
            }
            if let Some((id, started)) = track.active {
                if let Ok(status) = service.status(id)
                    && let Some(job) = status.first()
                {
                    if !job.finished() {
                        continue;
                    }
                    track.active = None;
                    if job.state == Stage::Failed {
                        track.state = State::Backoff;
                        track.changed();
                        track.not_before = now + track.backoff;
                        track.backoff = (track.backoff * 2).min(Duration::from_secs(300));
                    } else {
                        track.state = State::Watching;
                        track.backoff = Duration::from_secs(5);
                        track.not_before =
                            now + (started.elapsed() * 4).max(Duration::from_secs(2));
                        if job.state == Stage::Cancelled {
                            track.dirty = None;
                        }
                    }
                } else {
                    track.active = None;
                    track.state = State::Backoff;
                    track.changed();
                    track.not_before = now + track.backoff;
                }
            }
        }
        if let Some(id) = next_project(&tracks, now) {
            let track = tracks.get_mut(&id).expect("selected track");
            let Some(registry) = &previous else {
                continue;
            };
            let query = ScopeQuery {
                revision: registry.revision,
                id: track.project.id.clone(),
                approval: track.project.approval,
            };
            match service.submit_background(query) {
                Ok(job) => {
                    if let Ok(id) = job.id.parse() {
                        track.active = Some((id, now));
                        track.dirty = None;
                        track.state = State::Indexing;
                    }
                }
                Err(_) => {
                    track.state = State::Waiting;
                    track.not_before = now + Duration::from_secs(1);
                }
            }
        }
        if let Ok(mut status) = report.lock() {
            status.healthy = observer.is_some() && failed.is_empty();
            status.projects = tracks
                .values()
                .map(|track| Status {
                    id: track.project.id.clone(),
                    state: track.state,
                })
                .collect();
        }
        let _ = receiver.recv_timeout(Duration::from_millis(250));
    }
    for track in tracks.values() {
        cancel(track, &service);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_older_waiting_project_precedes_a_frequently_changed_project() {
        let now = Instant::now();
        let make = |id: &str, age: Duration| {
            let project:Project=serde_json::from_value(serde_json::json!({"id":id,"approval":7,"root":"source","apps":["com.example.Editor"],"enabled":true,"readings":{}})).unwrap();
            assert!(!project.monitoring);
            let mut track = Track::new(project);
            track.dirty = Some(now - age);
            track.not_before = now;
            track.quiet = now;
            track
        };
        let mut tracks = BTreeMap::from([
            ("a-hot".into(), make("a-hot", Duration::from_secs(1))),
            (
                "z-waiting".into(),
                make("z-waiting", Duration::from_secs(5)),
            ),
        ]);
        assert_eq!(next_project(&tracks, now).as_deref(), Some("z-waiting"));
        tracks.get_mut("z-waiting").unwrap().active = Some((7, now));
        assert_eq!(next_project(&tracks, now).as_deref(), Some("a-hot"));
    }
    #[test]
    fn notification_filter_preserves_ignore_file_changes_and_excludes_vcs_internals() {
        let root = Path::new("project");
        for (path, expected) in [
            ("project/.git/index", false),
            ("project/.git/info/exclude", true),
            ("project/.gitignore", true),
            ("project/build/main.rs", true),
            ("other/main.rs", false),
        ] {
            let event = Event::new(EventKind::Any).add_path(path.into());
            assert_eq!(relevant(root, &event), expected);
        }
    }
    #[test]
    #[ignore = "requires native notifications and the pinned analyzer; set KANADE_ASTRO_SIGHT"]
    #[cfg(not(windows))]
    fn monitoring_is_opt_in_restarts_and_stops_without_replacing_a_failed_generation() {
        fn wait(mut ready: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(45);
            while !ready() {
                assert!(Instant::now() < deadline, "monitor deadline exceeded");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
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
            root: root.clone(),
            directory_binding: Some(crate::directory_binding(&root).unwrap()),
            monitoring: false,
            apps: vec!["com.example.Editor".into()],
            enabled: true,
            readings: Default::default(),
        });
        registry.save(&data).unwrap();
        let service = Arc::new(Service::start(data.clone(), analyzer, || true).unwrap());
        let manager = Manager::start(data.clone(), service.clone()).unwrap();
        wait(|| manager.status().healthy);
        assert!(!data.join("sample/current.json").exists());
        let enable = |enabled| {
            let registry = Registry::load(&data).unwrap();
            crate::management::apply(
                &data,
                crate::management::Edit {
                    revision: registry.revision,
                    id: "sample".into(),
                    approval: 7,
                    operation: crate::management::Operation::SetMonitoring { enabled },
                },
            )
            .unwrap();
        };
        enable(true);
        let contains = |reading: &str, value: &str| {
            crate::Catalog::load(&data).is_ok_and(|catalog| {
                catalog
                    .lookup("com.example.Editor", reading)
                    .contains(&value.to_string())
            })
        };
        wait(|| contains("キャッシュリクエスト", "CacheRequest"));
        std::fs::write(root.join("two.rs"), "pub struct JsonParser;\n").unwrap();
        wait(|| contains("ジェイソンパーサー", "JsonParser"));
        let pointer = data.join("sample/current.json");
        let before = std::fs::read(&pointer).unwrap();
        std::fs::write(root.join("two.rs"), "pub struct Broken {\n").unwrap();
        wait(|| {
            manager
                .status()
                .projects
                .iter()
                .any(|p| p.state == State::Backoff)
        });
        assert_eq!(std::fs::read(&pointer).unwrap(), before);
        std::fs::write(root.join("two.rs"), "pub struct NetworkClient;\n").unwrap();
        wait(|| contains("ネットワーククライアント", "NetworkClient"));
        enable(false);
        wait(|| manager.status().projects.is_empty());
        let before = std::fs::read(&pointer).unwrap();
        std::fs::write(root.join("three.rs"), "pub struct DatabaseConnection;\n").unwrap();
        std::thread::sleep(Duration::from_millis(750));
        assert_eq!(std::fs::read(&pointer).unwrap(), before);
        drop(manager);
        enable(true);
        let restarted = Manager::start(data.clone(), service).unwrap();
        wait(|| contains("データベースコネクション", "DatabaseConnection"));
        std::fs::rename(&root, temp.path().join("old")).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("other.rs"), "pub struct Unapproved;\n").unwrap();
        wait(|| {
            restarted
                .status()
                .projects
                .iter()
                .any(|p| p.state == State::Unavailable)
        });
        let registry = Registry::load(&data).unwrap();
        assert!(crate::scan::index(&data, "sample", Path::new("unused"), None).is_err());
        assert!(registry.projects[0].monitoring);
        drop(restarted);
    }
}
