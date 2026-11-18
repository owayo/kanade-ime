// SPDX-License-Identifier: MIT
//! Project dictionaries contain approved vocabulary, never source or document excerpts.
use anyhow::{Context, Result, ensure};
use hasami::kana::reading_key;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
};
pub mod management;
pub mod monitor;
pub mod overrides;
mod readings;
pub mod scan;
pub mod watch;
pub mod workflow;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub id: String,
    #[serde(default)]
    pub approval: u64,
    pub root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory_binding: Option<String>,
    #[serde(default)]
    pub monitoring: bool,
    pub apps: Vec<String>,
    pub enabled: bool,
    #[serde(default)]
    pub readings: BTreeMap<String, String>,
}
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub revision: u64,
    pub projects: Vec<Project>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Term {
    pub reading: String,
    pub value: String,
}
#[derive(Default, Serialize, Deserialize)]
pub struct Generation {
    pub generation: u64,
    #[serde(default)]
    pub approval: u64,
    pub files: BTreeMap<String, Vec<Term>>,
}
#[derive(Default)]
pub struct Catalog {
    projects: Vec<ProjectTerms>,
}
#[derive(Clone)]
struct ProjectTerms {
    project: Project,
    terms: BTreeMap<String, Vec<String>>,
}
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let mut bytes = vec![];
    std::fs::File::open(path)?
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 8 * 1024 * 1024,
        "project data exceeds its limit"
    );
    Ok(serde_json::from_slice(&bytes)?)
}
pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= 8 * 1024 * 1024,
        "project data exceeds its limit"
    );
    let parent = path.parent().context("missing directory")?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut options = std::fs::File::options();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let (temporary, mut file) = loop {
        let sequence = SEQUENCE
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| std::io::Error::other("temporary sequence exhausted"))?;
        let temporary = path.with_extension(format!("{}-{sequence}.next", std::process::id()));
        match options.open(&temporary) {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    };
    let result = (|| -> Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}
impl Registry {
    pub fn load(data: &Path) -> Result<Self> {
        let path = data.join("registry.json");
        if !path.try_exists()? {
            return Ok(Self::default());
        }
        let registry: Self = read_json(&path)?;
        ensure!(registry.projects.len() <= 32, "too many projects");
        for p in &registry.projects {
            ensure!(
                valid_id(&p.id)
                    && !p.apps.is_empty()
                    && p.apps.len() <= 64
                    && p.apps.iter().all(|a| !a.is_empty() && a.len() <= 512),
                "invalid project approval"
            );
        }
        Ok(registry)
    }
    pub fn save(&mut self, data: &Path) -> Result<()> {
        let _lock = storage_lock(data, "registry")?;
        if Self::load(data)?.revision != self.revision {
            return Err(RegistryChanged.into());
        }
        let next = Self {
            revision: self.revision.checked_add(1).context("registry exhausted")?,
            projects: self.projects.clone(),
        };
        write_json(&data.join("registry.json"), &next)?;
        self.revision = next.revision;
        Ok(())
    }
}
#[derive(Debug)]
struct RegistryChanged;
impl std::fmt::Display for RegistryChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("project approvals changed; reload before saving")
    }
}
impl std::error::Error for RegistryChanged {}
struct StorageLock(std::fs::File);
impl Drop for StorageLock {
    fn drop(&mut self) {
        // A concurrent spawn can temporarily retain the open file description.
        let _ = self.0.unlock();
    }
}
fn storage_lock(data: &Path, name: &str) -> Result<StorageLock> {
    let directory = data.join(".locks");
    std::fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut options = std::fs::File::options();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(directory.join(format!("{name}.lock")))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock)
                if name == "registry" && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => return Err(error).context("project storage is busy"),
        }
    }
    Ok(StorageLock(file))
}
pub fn remove_project(data: &Path, id: &str) -> Result<()> {
    ensure!(valid_id(id), "invalid project ID");
    let mut registry = Registry::load(data)?;
    let removed_roots: Vec<_> = registry
        .projects
        .iter()
        .filter(|p| p.id == id)
        .map(|p| p.root.clone())
        .collect();
    if removed_roots.is_empty() {
        return Ok(());
    }
    registry.projects.retain(|p| p.id != id);
    registry.save(data)?;
    // Revocation is durable before cleanup. A running scan rechecks it before publishing.
    // Lock files live outside the removed folder and are never unlinked.
    management::cleanup(data, id, &removed_roots)?;
    Ok(())
}
struct RootIdentity(std::fs::Metadata);
impl RootIdentity {
    fn capture(root: &Path) -> Result<Self> {
        ensure!(root.canonicalize()? == root, "approved root changed");
        let metadata = std::fs::symlink_metadata(root)?;
        ensure!(metadata.is_dir(), "approved root is not a directory");
        Ok(Self(metadata))
    }
    fn matches(&self, root: &Path) -> bool {
        let Ok(current) = Self::capture(root) else {
            return false;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            self.0.dev() == current.0.dev() && self.0.ino() == current.0.ino()
        }
        #[cfg(not(unix))]
        {
            self.0
                .created()
                .ok()
                .is_some_and(|created| current.0.created().ok() == Some(created))
        }
    }
    fn binding(&self) -> Result<String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(format!("unix:{}:{}", self.0.dev(), self.0.ino()))
        }
        #[cfg(not(unix))]
        {
            let created = self.0.created()?.duration_since(std::time::UNIX_EPOCH)?;
            Ok(format!(
                "created:{}:{}",
                created.as_secs(),
                created.subsec_nanos()
            ))
        }
    }
}
pub fn directory_binding(root: &Path) -> Result<String> {
    RootIdentity::capture(root)?.binding()
}
impl Catalog {
    pub fn load(data: &Path) -> Result<Self> {
        Self::refresh(data, &Self::default()).map(|(catalog, _)| catalog)
    }
    pub fn refresh(data: &Path, previous: &Self) -> Result<(Self, bool)> {
        let registry = Registry::load(data)?;
        let mut catalog = Self::default();
        let mut retry = false;
        for project in registry.projects.iter().filter(|p| p.enabled) {
            if project.directory_binding.as_ref().is_some_and(|expected| {
                directory_binding(&project.root).as_ref().ok() != Some(expected)
            }) {
                continue;
            }
            match project_terms(data, project) {
                Ok(terms) => catalog.projects.push(ProjectTerms {
                    project: project.clone(),
                    terms,
                }),
                Err(_) => {
                    retry = true;
                    if let Some(old) = previous.projects.iter().find(|p| &p.project == project) {
                        catalog.projects.push(old.clone());
                    }
                }
            }
        }
        ensure!(
            Registry::load(data)? == registry,
            "project approvals changed during reload"
        );
        Ok((catalog, retry))
    }
    pub fn lookup(&self, app: &str, reading: &str) -> Vec<String> {
        let key = reading_key(reading);
        let mut values: Vec<_> = self
            .projects
            .iter()
            .filter(|p| p.project.apps.iter().any(|a| a == app))
            .filter_map(|p| p.terms.get(&key))
            .flatten()
            .take(16)
            .cloned()
            .collect();
        values.sort();
        values.dedup();
        values
    }
}
fn project_terms(data: &Path, project: &Project) -> Result<BTreeMap<String, Vec<String>>> {
    let folder = data.join(&project.id);
    let current: u64 = read_json(&folder.join("current.json"))?;
    let generation: Generation = read_json(&folder.join(format!("generation-{current}.json")))?;
    ensure!(
        generation.generation == current,
        "project generation mismatch"
    );
    let mut terms = BTreeMap::<String, Vec<String>>::new();
    if generation.approval == project.approval {
        for term in generation.files.into_values().flatten().take(50_000) {
            if valid_term(&term) {
                terms
                    .entry(reading_key(&term.reading))
                    .or_default()
                    .push(term.value);
            }
        }
    }
    for values in terms.values_mut() {
        values.sort();
        values.dedup();
    }
    Ok(terms)
}
pub fn valid_term(term: &Term) -> bool {
    !term.value.is_empty()
        && term.value.chars().count() <= 64
        && !term.value.chars().any(char::is_whitespace)
        && !term.reading.is_empty()
        && term.reading.chars().count() <= 128
        && reading_key(&term.reading)
            .chars()
            .all(|c| matches!(c, 'ァ'..='ヺ' | 'ー'))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interrupted_temporary_write_does_not_block_future_generations() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("current.json");
        std::fs::write(target.with_extension("next"), "incomplete").unwrap();
        write_json(&target, &42u64).unwrap();
        assert_eq!(read_json::<u64>(&target).unwrap(), 42);
        assert_eq!(
            std::fs::read_to_string(target.with_extension("next")).unwrap(),
            "incomplete"
        );
    }
    #[test]
    fn stale_registry_writes_and_parallel_index_locks_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let mut current = Registry::default();
        current.save(temp.path()).unwrap();
        let mut stale = Registry::load(temp.path()).unwrap();
        current.save(temp.path()).unwrap();
        assert!(stale.save(temp.path()).is_err());
        assert_eq!(
            Registry::load(temp.path()).unwrap().revision,
            current.revision
        );
        let lock = storage_lock(temp.path(), "index-sample").unwrap();
        assert!(storage_lock(temp.path(), "index-sample").is_err());
        let inherited = lock.0.try_clone().unwrap();
        drop(lock);
        assert!(storage_lock(temp.path(), "index-sample").is_ok());
        drop(inherited);
    }
    #[test]
    fn approval_does_not_follow_a_replaced_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let identity = RootIdentity::capture(&root).unwrap();
        std::fs::rename(&root, temp.path().join("old")).unwrap();
        std::fs::create_dir(&root).unwrap();
        assert!(!identity.matches(&root));
        #[cfg(unix)]
        {
            std::fs::remove_dir(&root).unwrap();
            std::os::unix::fs::symlink(temp.path().join("old"), &root).unwrap();
            assert!(!identity.matches(&root));
        }
    }
    #[test]
    fn generations_are_scoped_to_approved_applications() {
        let dir = tempfile::tempdir().unwrap();
        Registry {
            revision: 0,
            projects: vec![Project {
                id: "sample".into(),
                approval: 0,
                root: dir.path().into(),
                apps: vec!["editor".into()],
                enabled: true,
                monitoring: false,
                directory_binding: None,
                readings: BTreeMap::new(),
            }],
        }
        .save(dir.path())
        .unwrap();
        let folder = dir.path().join("sample");
        write_json(
            &folder.join("generation-1.json"),
            &Generation {
                generation: 1,
                approval: 0,
                files: BTreeMap::from([(
                    "a.rs".into(),
                    vec![Term {
                        reading: "にゅうりょく".into(),
                        value: "入力API".into(),
                    }],
                )]),
            },
        )
        .unwrap();
        write_json(&folder.join("current.json"), &1u64).unwrap();
        let catalog = Catalog::load(dir.path()).unwrap();
        assert_eq!(catalog.lookup("editor", "ニュウリョク"), ["入力API"]);
        assert!(catalog.lookup("other", "にゅうりょく").is_empty());
        let generation_path = folder.join("generation-1.json");
        let before = std::fs::read(&generation_path).unwrap();
        std::fs::write(&generation_path, "{").unwrap();
        let (retained, retry) = Catalog::refresh(dir.path(), &catalog).unwrap();
        assert!(retry);
        assert_eq!(retained.lookup("editor", "ニュウリョク"), ["入力API"]);
        let mut registry = Registry::load(dir.path()).unwrap();
        registry.projects[0].approval = 1;
        registry.save(dir.path()).unwrap();
        assert!(
            Catalog::refresh(dir.path(), &retained)
                .unwrap()
                .0
                .lookup("editor", "ニュウリョク")
                .is_empty()
        );
        std::fs::write(&generation_path, before).unwrap();
        assert!(
            Catalog::load(dir.path())
                .unwrap()
                .lookup("editor", "ニュウリョク")
                .is_empty()
        );
        assert!(!valid_id("../sample"));
    }
}
