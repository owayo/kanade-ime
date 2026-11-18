//! OS notifications are coalesced off the input path. Event payloads are never logged.
use crate::{Registry, scan, storage_lock};
use anyhow::{Context, Result, ensure};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    path::{Component, Path},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    time::{Duration, Instant},
};

pub struct Changes {
    _watcher: RecommendedWatcher,
    receiver: Receiver<()>,
}
impl Changes {
    pub fn new(paths: &[&Path], include: impl Fn(&Event) -> bool + Send + 'static) -> Result<Self> {
        ensure!(
            !cfg!(windows),
            "native project notifications are unavailable on Windows; use explicit index and reload"
        );
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut watcher = RecommendedWatcher::new(
            move |event: notify::Result<Event>| {
                let relevant = match event {
                    Ok(event) => {
                        !matches!(event.kind, EventKind::Access(_))
                            && (event.need_rescan() || include(&event))
                    }
                    Err(_) => true,
                };
                if relevant {
                    // One pending wakeup suffices: consumers reload a complete snapshot.
                    let _ = sender.try_send(());
                }
            },
            notify::Config::default().with_follow_symlinks(false),
        )?;
        for path in paths {
            watcher.watch(path, RecursiveMode::Recursive)?;
        }
        Ok(Self {
            _watcher: watcher,
            receiver,
        })
    }

    pub fn wait(&self, timeout: Duration) -> Result<bool> {
        match self.receiver.recv_timeout(timeout) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => return Ok(false),
            Err(RecvTimeoutError::Disconnected) => anyhow::bail!("project watcher stopped"),
        }
        // Quiet saves for 500 ms, but continuous activity cannot postpone an update forever.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self
                .receiver
                .recv_timeout(remaining.min(Duration::from_millis(500)))
            {
                Ok(()) => {}
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => anyhow::bail!("project watcher stopped"),
            }
        }
        Ok(true)
    }
}

pub fn catalog_changes(data: &Path) -> Result<Changes> {
    std::fs::create_dir_all(data)?;
    let root = data.canonicalize()?;
    let watched = root.clone();
    Changes::new(&[&root], move |event| {
        event.paths.is_empty()
            || event.paths.iter().any(|p| {
                p == &watched
                    || p.file_name()
                        .is_some_and(|name| name == "registry.json" || name == "current.json")
                    || matches!(event.kind, EventKind::Remove(_))
            })
    })
}

pub fn run(data: &Path, id: &str, analyzer: &Path) -> Result<()> {
    ensure!(crate::valid_id(id), "invalid project ID");
    let _watch_lock = storage_lock(data, &format!("watch-{id}"))?;
    let registry = Registry::load(data)?;
    let project = registry
        .projects
        .iter()
        .find(|p| p.id == id && p.enabled)
        .context("project has not been approved")?;
    let root = project.root.canonicalize()?;
    ensure!(root == project.root, "approved root changed");
    let identity = crate::RootIdentity::capture(&root)?;
    let data = data.canonicalize()?;
    ensure!(
        !data.starts_with(&root) && !root.starts_with(&data),
        "project data must be outside its source root"
    );
    #[cfg(unix)]
    ensure!(
        unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10) } == 0,
        "could not lower project analysis priority"
    );
    let source = root.clone();
    let state = data.clone();
    let changes = Changes::new(&[&root, &data], move |event| {
        event.paths.is_empty()
            || event.paths.iter().any(|path| {
                if path.starts_with(&state) {
                    return (path == &state && matches!(event.kind, EventKind::Remove(_)))
                        || path.file_name().is_some_and(|n| n == "registry.json");
                }
                path.strip_prefix(&source).is_ok_and(|relative| {
                    // Exclusion rules belong to the analyzer. Even build/dist can be
                    // legitimate tracked source; only VCS internals are never source.
                    !relative.components().any(|part| {
                        matches!(part,
                        Component::Normal(n) if matches!(n.to_str(), Some(".git" | ".hg" | ".svn")))
                    }) || relative == Path::new(".git/info/exclude")
                })
            })
    })?;
    // Register notifications first, so saves during the initial scan remain pending.
    let initial = Instant::now();
    scan::index(&data, id, analyzer, None)?;
    let mut not_before = Instant::now() + (initial.elapsed() * 4).max(Duration::from_secs(2));
    let mut pending = false;
    let mut backoff = Duration::from_secs(5);
    println!("watching approved project");
    loop {
        let timeout = if pending {
            not_before
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(300))
        } else {
            Duration::from_secs(300)
        };
        pending |= changes.wait(timeout)?;
        let approved = Registry::load(&data)?;
        if !approved
            .projects
            .iter()
            .any(|p| p.id == id && p.enabled && p.root == root && p.approval == project.approval)
            || !identity.matches(&root)
        {
            println!("project watch stopped: approval or root changed");
            return Ok(());
        }
        if pending && Instant::now() >= not_before {
            // Full scans preserve identical ignore rules after renames, deletions and
            // ignore-file edits. Incremental analyzer exclusions are not yet equivalent.
            let started = Instant::now();
            match scan::index(&data, id, analyzer, None) {
                Ok(_) => {
                    pending = false;
                    backoff = Duration::from_secs(5);
                    not_before =
                        Instant::now() + (started.elapsed() * 4).max(Duration::from_secs(2));
                    println!("project dictionary updated");
                }
                Err(_) => {
                    not_before = Instant::now() + backoff;
                    backoff = (backoff * 2).min(Duration::from_secs(300));
                    eprintln!("project update failed; previous generation retained");
                }
            }
        }
    }
}
