//! Revision-bound management of existing approvals. New directories must be
//! approved separately; application scopes change only through an explicit edit.
use crate::{Registry, storage_lock, valid_id};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub revision: u64,
    pub id: String,
    pub approval: u64,
    pub operation: Operation,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    SetEnabled { enabled: bool },
    SetApps { apps: Vec<String> },
    SetMonitoring { enabled: bool },
    Revoke,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeQuery {
    pub revision: u64,
    pub id: String,
    pub approval: u64,
}

/// A bounded scope snapshot, without source terms or reading overrides.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppScope {
    pub revision: u64,
    pub id: String,
    pub approval: u64,
    pub root: String,
    pub apps: Vec<String>,
}

pub fn valid_apps(apps: &[String]) -> bool {
    !apps.is_empty()
        && apps.len() <= 64
        && apps.iter().all(|app| {
            !app.trim().is_empty() && app.len() <= 512 && !app.chars().any(char::is_control)
        })
}

pub fn app_scope(registry: &Registry, query: &ScopeQuery) -> Result<AppScope, EditError> {
    if !valid_id(&query.id) {
        return Err(EditError::Invalid);
    }
    if registry.revision != query.revision {
        return Err(EditError::Stale);
    }
    let project = registry
        .projects
        .iter()
        .find(|project| project.id == query.id)
        .ok_or(EditError::NotFound)?;
    if project.approval != query.approval {
        return Err(EditError::Stale);
    }
    if !valid_apps(&project.apps) {
        return Err(EditError::Invalid);
    }
    Ok(AppScope {
        revision: registry.revision,
        id: project.id.clone(),
        approval: project.approval,
        root: project.root.to_string_lossy().chars().take(1024).collect(),
        apps: project.apps.clone(),
    })
}

#[derive(Serialize, Deserialize)]
pub struct Outcome {
    pub listing: Listing,
    pub cleanup_complete: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Summary {
    pub id: String,
    pub approval: u64,
    pub root: String,
    pub enabled: bool,
    pub apps: usize,
    pub readings: usize,
    #[serde(default)]
    pub monitoring: bool,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Listing {
    pub revision: u64,
    pub projects: Vec<Summary>,
}
impl From<&Registry> for Listing {
    fn from(registry: &Registry) -> Self {
        Self {
            revision: registry.revision,
            projects: registry
                .projects
                .iter()
                .map(|p| Summary {
                    id: p.id.clone(),
                    approval: p.approval,
                    enabled: p.enabled,
                    root: p.root.to_string_lossy().chars().take(1024).collect(),
                    apps: p.apps.len(),
                    readings: p.readings.len(),
                    monitoring: p.monitoring,
                })
                .collect(),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditState {
    Applied,
    Stale,
    NotFound,
    Busy,
    Invalid,
    Unavailable,
}
#[derive(Serialize, Deserialize)]
pub struct Reply {
    pub result: EditState,
    pub listing: Option<Listing>,
    pub cleanup_complete: bool,
    pub reloaded: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditError {
    Stale,
    NotFound,
    Busy,
    Invalid,
    Unavailable,
}
impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "project edit failed: {self:?}")
    }
}
impl std::error::Error for EditError {}
fn storage_error(error: anyhow::Error) -> EditError {
    if error.is::<crate::RegistryChanged>() {
        EditError::Stale
    } else if error.is::<std::fs::TryLockError>() {
        EditError::Busy
    } else {
        EditError::Unavailable
    }
}
pub fn apply(data: &Path, edit: Edit) -> std::result::Result<Outcome, EditError> {
    if !valid_id(&edit.id) {
        return Err(EditError::Invalid);
    }
    let mut registry = Registry::load(data).map_err(storage_error)?;
    if registry.revision != edit.revision {
        return Err(EditError::Stale);
    }
    let position = registry
        .projects
        .iter()
        .position(|p| p.id == edit.id)
        .ok_or(EditError::NotFound)?;
    if registry.projects[position].approval != edit.approval {
        return Err(EditError::Stale);
    }
    let protected = registry.projects[position].root.clone();
    let revoke = matches!(edit.operation, Operation::Revoke);
    match edit.operation {
        Operation::SetEnabled { enabled } => registry.projects[position].enabled = enabled,
        Operation::SetApps { apps } => {
            if !valid_apps(&apps) {
                return Err(EditError::Invalid);
            }
            registry.projects[position].apps = apps;
        }
        Operation::SetMonitoring { enabled } => {
            if enabled {
                if cfg!(windows) || !registry.projects[position].enabled {
                    return Err(EditError::Invalid);
                }
                let root = &registry.projects[position].root;
                let resolved = root.canonicalize().map_err(|_| EditError::Unavailable)?;
                let storage = data.canonicalize().map_err(|_| EditError::Unavailable)?;
                if &resolved != root || root.starts_with(&storage) || storage.starts_with(root) {
                    return Err(EditError::Invalid);
                }
                let binding = crate::directory_binding(root).map_err(|_| EditError::Unavailable)?;
                if registry.projects[position]
                    .directory_binding
                    .as_ref()
                    .is_some_and(|expected| expected != &binding)
                {
                    return Err(EditError::Stale);
                }
                registry.projects[position].directory_binding = Some(binding);
            }
            registry.projects[position].monitoring = enabled;
        }
        Operation::Revoke => {
            registry.projects.remove(position);
        }
    }
    // save compares the revision again under the writer lock, preventing a race
    // with CLI changes or another settings window.
    registry.save(data).map_err(storage_error)?;
    let cleanup_complete = !revoke || cleanup(data, &edit.id, &[protected]).unwrap_or(false);
    Ok(Outcome {
        listing: Listing::from(&registry),
        cleanup_complete,
    })
}

pub(crate) fn cleanup(data: &Path, id: &str, removed_roots: &[PathBuf]) -> Result<bool> {
    let _lock = storage_lock(data, "registry")?;
    let registry = Registry::load(data)?;
    if registry.projects.iter().any(|p| p.id == id) {
        return Ok(false);
    }
    let folder = data.join(id);
    if !folder.try_exists()? {
        return Ok(true);
    }
    if !std::fs::symlink_metadata(&folder)?.is_dir() {
        return Ok(false);
    }
    let resolved = folder.canonicalize()?;
    // Even an invalid legacy approval must never turn revocation into source
    // deletion. Generation cleanup is secondary to durable revocation.
    for root in removed_roots
        .iter()
        .chain(registry.projects.iter().map(|p| &p.root))
    {
        let Ok(root) = root.canonicalize() else {
            return Ok(false);
        };
        if root.starts_with(&resolved) || resolved.starts_with(&root) {
            return Ok(false);
        }
    }
    for entry in std::fs::read_dir(&folder)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() || !generated_filename(&entry.file_name()) {
            return Ok(false);
        }
    }
    std::fs::remove_dir_all(folder)?;
    Ok(true)
}

fn generated_filename(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if name == "current.json" {
        return true;
    }
    if let Some(number) = name
        .strip_prefix("generation-")
        .and_then(|s| s.strip_suffix(".json"))
    {
        return !number.is_empty() && number.bytes().all(|c| c.is_ascii_digit());
    }
    name.strip_suffix(".next")
        .and_then(|s| {
            s.strip_prefix("current.")
                .or_else(|| s.strip_prefix("generation-"))
        })
        .is_some_and(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'-'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Project;
    fn fixture() -> (tempfile::TempDir, Registry) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        std::fs::create_dir(&root).unwrap();
        let mut registry = Registry::default();
        registry.projects.push(Project {
            id: "sample".into(),
            approval: 7,
            root,
            apps: vec!["com.example.Editor".into()],
            enabled: true,
            monitoring: false,
            directory_binding: None,
            readings: Default::default(),
        });
        registry.save(&temp.path().join("data")).unwrap();
        (temp, registry)
    }
    fn edit(registry: &Registry, operation: Operation) -> Edit {
        Edit {
            revision: registry.revision,
            id: "sample".into(),
            approval: 7,
            operation,
        }
    }
    #[test]
    fn stale_lists_and_replaced_approvals_cannot_change_access() {
        let (temp, registry) = fixture();
        let data = temp.path().join("data");
        let outcome = apply(
            &data,
            edit(&registry, Operation::SetEnabled { enabled: false }),
        )
        .unwrap();
        assert!(!outcome.listing.projects[0].enabled);
        assert_eq!(
            Registry::load(&data).unwrap().projects[0].apps,
            registry.projects[0].apps
        );
        assert!(apply(&data, edit(&registry, Operation::Revoke)).is_err());
        let mut latest = Registry::load(&data).unwrap();
        latest.projects[0].approval = 8;
        latest.save(&data).unwrap();
        let before = std::fs::read(data.join("registry.json")).unwrap();
        assert!(apply(&data, edit(&latest, Operation::Revoke)).is_err());
        assert_eq!(std::fs::read(data.join("registry.json")).unwrap(), before);
    }
    #[test]
    fn revocation_cleans_generations_and_preserves_source_even_when_paths_overlap() {
        let (temp, mut registry) = fixture();
        let data = temp.path().join("data");
        let folder = data.join("sample");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("generation-1.json"), "fixture").unwrap();
        let outcome = apply(&data, edit(&registry, Operation::Revoke)).unwrap();
        assert!(outcome.cleanup_complete && outcome.listing.projects.is_empty());
        assert!(registry.projects[0].root.is_dir());
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("source.rs"), "pub struct Example;").unwrap();
        registry = Registry::load(&data).unwrap();
        registry.projects.push(Project {
            id: "sample".into(),
            approval: 7,
            root: folder.clone(),
            apps: vec!["com.example.Editor".into()],
            enabled: true,
            monitoring: false,
            directory_binding: None,
            readings: Default::default(),
        });
        registry.save(&data).unwrap();
        let outcome = apply(&data, edit(&registry, Operation::Revoke)).unwrap();
        assert!(!outcome.cleanup_complete && outcome.listing.projects.is_empty());
        assert!(folder.join("source.rs").is_file());
        crate::remove_project(&data, "sample").unwrap();
        assert!(folder.join("source.rs").is_file());
    }
    #[test]
    fn invalid_actions_and_ids_leave_the_registry_unchanged() {
        let (temp, registry) = fixture();
        let data = temp.path().join("data");
        let mut bad = edit(&registry, Operation::Revoke);
        bad.id = "../source".into();
        assert!(apply(&data, bad).is_err());
        assert!(
            serde_json::from_str::<Operation>(
                r#"{"kind":"set_enabled","enabled":true,"root":"other"}"#
            )
            .is_err()
        );
        assert!(serde_json::from_str::<Operation>(r#"{"kind":"approve"}"#).is_err());
        assert!(Registry::load(&data).unwrap() == registry);
    }
    #[test]
    fn scope_replacement_is_revision_bound_and_preserves_other_approval_data() {
        let (temp, mut registry) = fixture();
        let data = temp.path().join("data");
        registry.projects[0]
            .readings
            .insert("CacheRequest".into(), "キャッシュ".into());
        registry.save(&data).unwrap();
        let query = ScopeQuery {
            revision: registry.revision,
            id: "sample".into(),
            approval: 7,
        };
        let scope = app_scope(&registry, &query).unwrap();
        assert_eq!(scope.apps, ["com.example.Editor"]);
        let apps = vec!["com.example.Terminal".into(), "com.example.Other".into()];
        let proposal = edit(&registry, Operation::SetApps { apps: apps.clone() });
        apply(&data, proposal.clone()).unwrap();
        let changed = Registry::load(&data).unwrap();
        assert_eq!(changed.projects[0].apps, apps);
        let mut expected = registry.projects[0].clone();
        expected.apps = apps;
        assert_eq!(changed.projects[0], expected);
        assert!(matches!(apply(&data, proposal), Err(EditError::Stale)));
        assert!(matches!(app_scope(&changed, &query), Err(EditError::Stale)));
        let before = std::fs::read(data.join("registry.json")).unwrap();
        for apps in [
            vec![],
            vec![" ".into()],
            vec!["bad\nID".into()],
            vec!["a".repeat(513)],
            vec!["a".into(); 65],
        ] {
            assert!(matches!(
                apply(&data, edit(&changed, Operation::SetApps { apps })),
                Err(EditError::Invalid)
            ));
        }
        let mut wrong = edit(
            &changed,
            Operation::SetApps {
                apps: vec!["com.example.Editor".into()],
            },
        );
        wrong.approval += 1;
        assert!(matches!(apply(&data, wrong), Err(EditError::Stale)));
        assert_eq!(std::fs::read(data.join("registry.json")).unwrap(), before);
    }
    #[test]
    fn scope_details_are_bounded_and_exclude_reading_overrides() {
        let (_temp, mut registry) = fixture();
        registry.projects[0].apps = vec!["a".repeat(512); 64];
        registry.projects[0]
            .readings
            .insert("PrivateIdentifier".into(), "サンプル".into());
        let query = ScopeQuery {
            revision: registry.revision,
            id: "sample".into(),
            approval: 7,
        };
        let bytes = serde_json::to_vec(&app_scope(&registry, &query).unwrap()).unwrap();
        assert!(bytes.len() < 40 * 1024);
        assert!(!bytes.windows(17).any(|s| s == b"PrivateIdentifier"));
        let mut wrong = query;
        wrong.approval += 1;
        assert!(matches!(
            app_scope(&registry, &wrong),
            Err(EditError::Stale)
        ));
        assert!(
            serde_json::from_str::<ScopeQuery>(
                r#"{"revision":1,"id":"sample","approval":7,"root":"other"}"#
            )
            .is_err()
        );
    }
    #[test]
    fn management_metadata_is_bounded_independently_of_reading_overrides() {
        let (_temp, mut registry) = fixture();
        for i in 0..10000 {
            registry.projects[0]
                .readings
                .insert(format!("Identifier{i}"), "サンプル".into());
        }
        registry.projects[0].apps = vec!["a".repeat(512); 64];
        let mut listing = Listing::from(&registry);
        assert_eq!(listing.projects[0].readings, 10000);
        assert_eq!(listing.projects[0].apps, 64);
        listing.projects = vec![listing.projects[0].clone(); 32];
        let encoded = serde_json::to_vec(&listing).unwrap();
        assert!(encoded.len() < 256 * 1024);
        assert!(!encoded.windows(10).any(|s| s == b"Identifier"));
    }
}
