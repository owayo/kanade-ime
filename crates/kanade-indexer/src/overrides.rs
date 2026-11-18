//! Explicit, paginated reading edits. Vocabulary is returned only to the editor.
use crate::{Project, Registry, Term, management::ScopeQuery, valid_term};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub scope: ScopeQuery,
    pub offset: u32,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub identifier: String,
    pub reading: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub query: Query,
    pub total: usize,
    pub entries: Vec<Entry>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub query: ScopeQuery,
    pub identifier: String,
    pub reading: Option<String>,
}
impl Edit {
    pub fn valid(&self) -> bool {
        valid_term(&Term {
            value: self.identifier.clone(),
            reading: self.reading.clone().unwrap_or_else(|| "ア".into()),
        }) && !self.identifier.chars().any(char::is_control)
    }
}
fn project<'a>(registry: &'a Registry, query: &ScopeQuery) -> Result<&'a Project> {
    ensure!(registry.revision == query.revision, "project changed");
    registry
        .projects
        .iter()
        .find(|p| p.id == query.id && p.approval == query.approval)
        .ok_or_else(|| anyhow::anyhow!("project changed"))
}
pub fn page(data: &Path, query: Query) -> Result<Page> {
    let registry = Registry::load(data)?;
    let project = project(&registry, &query.scope)?;
    let entries: Vec<Entry> = project
        .readings
        .iter()
        .skip(query.offset as usize)
        .take(50)
        .map(|(identifier, reading)| Entry {
            identifier: identifier.clone(),
            reading: reading.clone(),
        })
        .collect();
    ensure!(
        entries.iter().all(|entry| valid_term(&Term {
            value: entry.identifier.clone(),
            reading: entry.reading.clone()
        }) && !entry.identifier.chars().any(char::is_control)),
        "invalid stored reading"
    );
    Ok(Page {
        query,
        total: project.readings.len(),
        entries,
    })
}
pub(crate) fn apply(data: &Path, edit: Edit) -> Result<(Project, u64)> {
    ensure!(edit.valid(), "invalid reading edit");
    let mut registry = Registry::load(data)?;
    let snapshot = project(&registry, &edit.query)?;
    ensure!(snapshot.enabled, "project disabled");
    if let Some(binding) = &snapshot.directory_binding {
        ensure!(
            &crate::directory_binding(&snapshot.root)? == binding,
            "approved directory changed"
        );
    }
    let target = registry
        .projects
        .iter_mut()
        .find(|p| p.id == edit.query.id)
        .expect("checked project");
    match edit.reading {
        Some(reading) => {
            ensure!(
                target.readings.len() < 10_000 || target.readings.contains_key(&edit.identifier),
                "too many overrides"
            );
            target.readings.insert(edit.identifier, reading);
        }
        None => {
            ensure!(
                target.readings.remove(&edit.identifier).is_some(),
                "override unavailable"
            );
        }
    }
    let target = target.clone();
    registry.save(data)?;
    Ok((target, registry.revision))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pages_and_edits_are_revision_bound_and_preserve_approval_scope() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        std::fs::create_dir(&root).unwrap();
        let data = temp.path().join("data");
        let mut registry = Registry::default();
        registry.projects.push(Project {
            id: "sample".into(),
            approval: 7,
            root,
            monitoring: false,
            directory_binding: None,
            apps: vec!["com.example.Editor".into()],
            enabled: true,
            readings: (0..101)
                .map(|i| (format!("Identifier{i:03}"), "ヨミ".into()))
                .collect(),
        });
        registry.save(&data).unwrap();
        let scope = ScopeQuery {
            revision: registry.revision,
            id: "sample".into(),
            approval: 7,
        };
        let second = page(
            &data,
            Query {
                scope: scope.clone(),
                offset: 50,
            },
        )
        .unwrap();
        assert_eq!(second.total, 101);
        assert_eq!(second.entries.len(), 50);
        assert_eq!(second.entries[0].identifier, "Identifier050");
        assert!(serde_json::to_vec(&second).unwrap().len() < 64 * 1024);
        let edit = Edit {
            query: scope.clone(),
            identifier: "CacheRequest".into(),
            reading: Some("キャッシュ".into()),
        };
        let (changed, revision) = apply(&data, edit.clone()).unwrap();
        let mut expected = registry.projects[0].clone();
        expected
            .readings
            .insert("CacheRequest".into(), "キャッシュ".into());
        assert_eq!(changed, expected);
        assert!(apply(&data, edit).is_err());
        assert!(
            page(
                &data,
                Query {
                    scope: scope.clone(),
                    offset: 0
                }
            )
            .is_err()
        );
        let mut query = scope;
        query.revision = revision;
        query.approval += 1;
        assert!(
            apply(
                &data,
                Edit {
                    query: query.clone(),
                    identifier: "CacheRequest".into(),
                    reading: None
                }
            )
            .is_err()
        );
        query.approval -= 1;
        let (reset, _) = apply(
            &data,
            Edit {
                query,
                identifier: "CacheRequest".into(),
                reading: None,
            },
        )
        .unwrap();
        assert!(!reset.readings.contains_key("CacheRequest"));
    }
    #[test]
    fn reading_validation_rejects_non_kana_and_control_identifiers() {
        let query = ScopeQuery {
            revision: 0,
            id: "sample".into(),
            approval: 7,
        };
        for (identifier, reading) in [
            ("CacheRequest", "alphabet"),
            ("Bad\0Name", "ヨミ"),
            ("", "ヨミ"),
            ("CacheRequest", ""),
        ] {
            assert!(
                !Edit {
                    query: query.clone(),
                    identifier: identifier.into(),
                    reading: Some(reading.into())
                }
                .valid()
            );
        }
    }
}
