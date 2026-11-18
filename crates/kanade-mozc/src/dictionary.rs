//! Explicit dictionary administration. Callers must keep disk access off key handling.
use crate::mozc::user_dictionary as pb;
use prost::Message;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read},
    path::Path,
};

const MAX_BYTES: u64 = 8 * 1024 * 1024;
const PAGE_SIZE: usize = 50;
mod edit;
pub use edit::{Edit, NewEntry, Prepared, prepare};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReloadState {
    Idle,
    Loading,
    Ready,
    Failed,
    Busy,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reload {
    pub generation: u64,
    pub state: ReloadState,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("dictionary storage is unavailable")]
    Io(#[from] io::Error),
    #[error("dictionary storage is invalid or exceeds its limit")]
    Invalid,
    #[error("dictionary changed; refresh before editing")]
    Conflict,
    #[error("dictionary contains an unsupported storage representation")]
    Unsupported,
    #[error("another dictionary edit is in progress")]
    Busy,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DictionaryInfo {
    pub index: u32,
    pub name: String,
    pub entries: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub dictionary_index: u32,
    pub entry_index: u32,
    pub reading: String,
    pub value: String,
    pub pos: Option<String>,
    pub comment: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Page {
    pub revision: String,
    pub dictionaries: Vec<DictionaryInfo>,
    pub entries: Vec<Entry>,
    pub offset: u32,
    pub total: u32,
}
fn load(profile: &Path) -> Result<(Vec<u8>, pb::UserDictionaryStorage), Error> {
    let path = profile.join("user_dictionary.db");
    let mut bytes = vec![];
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.len() > MAX_BYTES {
                return Err(Error::Invalid);
            }
            File::open(path)?
                .take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err(Error::Invalid);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let storage =
        pb::UserDictionaryStorage::decode(bytes.as_slice()).map_err(|_| Error::Invalid)?;
    validate_storage(&storage)?;
    Ok((bytes, storage))
}
fn validate_storage(storage: &pb::UserDictionaryStorage) -> Result<(), Error> {
    let total: usize = storage.dictionaries.iter().map(|d| d.entries.len()).sum();
    if storage.dictionaries.len() > 100
        || total > 100_000
        || storage.dictionaries.iter().any(|d| {
            d.name.as_ref().is_some_and(|n| n.len() > 300)
                || d.entries.iter().any(|e| {
                    [&e.key, &e.value, &e.comment]
                        .iter()
                        .any(|s| s.as_ref().is_some_and(|s| s.len() > 300))
                })
        })
    {
        return Err(Error::Invalid);
    }
    Ok(())
}
fn revision(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub fn read(profile: &Path, offset: u32) -> Result<Page, Error> {
    let (bytes, storage) = load(profile)?;
    let revision = revision(&bytes);
    let total: usize = storage.dictionaries.iter().map(|d| d.entries.len()).sum();
    let dictionaries = storage
        .dictionaries
        .iter()
        .enumerate()
        .map(|(index, d)| DictionaryInfo {
            index: index as u32,
            name: d.name.clone().unwrap_or_default(),
            entries: d.entries.len() as u32,
        })
        .collect();
    let entries = storage
        .dictionaries
        .into_iter()
        .enumerate()
        .flat_map(|(dictionary, d)| {
            d.entries
                .into_iter()
                .enumerate()
                .map(move |(index, e)| Entry {
                    dictionary_index: dictionary as u32,
                    entry_index: index as u32,
                    reading: e.key.unwrap_or_default(),
                    value: e.value.unwrap_or_default(),
                    pos: e
                        .pos
                        .and_then(|p| pb::user_dictionary::PosType::try_from(p).ok())
                        .map(|p| p.as_str_name().to_ascii_lowercase()),
                    comment: e.comment.unwrap_or_default(),
                })
        })
        .skip(offset as usize)
        .take(PAGE_SIZE)
        .collect();
    Ok(Page {
        revision,
        dictionaries,
        entries,
        offset,
        total: total as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_older_worker_is_rejected_before_it_can_receive_a_dictionary_control() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("worker");
        std::fs::write(&binary, b"fixture").unwrap();
        let lock: toml::Value =
            toml::from_str(include_str!("../../../locks/mozc.lock.toml")).unwrap();
        let mut manifest = serde_json::json!({
            "mozc_commit":lock["mozc"]["commit"].as_str(),
            "tag":lock["mozc"]["tag"].as_str(),
            "sha256":crate::artifact_hash(&binary).unwrap(),
            "worker_protocol":2,
        });
        let path = binary.with_file_name("manifest.json");
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        crate::verify_artifact(&binary).unwrap();
        manifest.as_object_mut().unwrap().remove("worker_protocol");
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(matches!(
            crate::verify_artifact(&binary),
            Err(crate::Error::Version)
        ));
        manifest["worker_protocol"] = serde_json::json!(1);
        std::fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(matches!(
            crate::verify_artifact(&binary),
            Err(crate::Error::Version)
        ));
    }
    #[test]
    #[ignore = "requires the locked worker; make contract"]
    fn dictionary_only_reload_preserves_composition_and_unsynced_prediction() {
        use crate::{ConversionEngine, Mozc, SessionId, Snapshot};
        use kanade_ipc::{Key, Privacy, SpecialKey};
        use std::time::{Duration, Instant};
        fn type_text(worker: &mut Mozc, id: SessionId, text: &str) -> Snapshot {
            let mut result = Snapshot::default();
            for ch in text.chars() {
                result = worker
                    .key(
                        id,
                        &Key {
                            text: ch.to_string(),
                            ..Default::default()
                        },
                        "",
                    )
                    .unwrap();
            }
            result
        }
        fn wait(worker: &mut Mozc, generation: u64, expected: ReloadState) {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let result = worker.dictionary_reload_status().unwrap();
                assert_eq!(result.generation, generation);
                if result.state == expected {
                    return;
                }
                assert_eq!(result.state, ReloadState::Loading);
                assert!(
                    Instant::now() < deadline,
                    "dictionary reload deadline exceeded"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let profile = tempfile::tempdir().unwrap();
        let mut worker = Mozc::start(&crate::default_worker(), profile.path()).unwrap();
        assert_eq!(
            worker.dictionary_reload_status().unwrap(),
            Reload {
                generation: 0,
                state: ReloadState::Idle
            }
        );
        worker.reload_dictionary(1).unwrap();
        wait(&mut worker, 1, ReloadState::Failed);
        worker.set_privacy(Privacy::Normal).unwrap();
        let id = worker.create_session().unwrap();
        worker
            .add_history("かなでれきし", "KanadeHistoryFixture")
            .unwrap();
        let storage = pb::UserDictionaryStorage {
            dictionaries: vec![pb::UserDictionary {
                id: Some(1),
                name: Some("Fixture".into()),
                entries: vec![pb::user_dictionary::Entry {
                    key: Some("かなでじしょ".into()),
                    value: Some("KanadeDictionaryFixture".into()),
                    pos: Some(pb::user_dictionary::PosType::Noun as i32),
                    ..Default::default()
                }],
            }],
            ..Default::default()
        };
        let path = profile.path().join("user_dictionary.db");
        std::fs::write(&path, storage.encode_to_vec()).unwrap();
        type_text(&mut worker, id, "kana");
        worker.reload_dictionary(2).unwrap();
        let continued = type_text(&mut worker, id, "dejisho");
        assert!(kanade_ipc::reading(&continued.preedit) == "かなでじしょ");
        wait(&mut worker, 2, ReloadState::Ready);
        worker.reset(id).unwrap();
        type_text(&mut worker, id, "kanadejisho");
        let converted = worker
            .key(
                id,
                &Key {
                    special: SpecialKey::Space as i32,
                    ..Default::default()
                },
                "",
            )
            .unwrap();
        assert!(
            converted
                .candidates
                .iter()
                .any(|c| c.value == "KanadeDictionaryFixture")
        );
        worker.reset(id).unwrap();
        assert!(
            type_text(&mut worker, id, "kanaderekishi")
                .candidates
                .iter()
                .any(|c| c.value == "KanadeHistoryFixture")
        );
        worker.reset(id).unwrap();
        std::fs::write(&path, [0xff]).unwrap();
        // Repeating a generation returns its cached completion, without loading a new file.
        assert_eq!(
            worker.reload_dictionary(2).unwrap().state,
            ReloadState::Ready
        );
        worker.reload_dictionary(3).unwrap();
        wait(&mut worker, 3, ReloadState::Failed);
        type_text(&mut worker, id, "kanadejisho");
        let retained = worker
            .key(
                id,
                &Key {
                    special: SpecialKey::Space as i32,
                    ..Default::default()
                },
                "",
            )
            .unwrap();
        assert!(
            retained
                .candidates
                .iter()
                .any(|c| c.value == "KanadeDictionaryFixture")
        );
        worker.reset(id).unwrap();
        std::fs::write(path, storage.encode_to_vec()).unwrap();
        worker.reload_dictionary(4).unwrap();
        wait(&mut worker, 4, ReloadState::Ready);
    }
    #[test]
    #[ignore = "requires the locked worker; make contract"]
    fn pinned_dictionary_fixture_is_visible_to_the_worker_without_rewriting_storage() {
        use crate::{ConversionEngine, Mozc};
        use kanade_ipc::{Key, Privacy, SpecialKey};
        use std::time::{Duration, Instant};
        let profile = tempfile::tempdir().unwrap();
        let storage = pb::UserDictionaryStorage {
            dictionaries: vec![pb::UserDictionary {
                id: Some(1),
                name: Some("Fixture".into()),
                entries: vec![pb::user_dictionary::Entry {
                    key: Some("かなでじしょ".into()),
                    value: Some("KanadeDictionaryFixture".into()),
                    pos: Some(pb::user_dictionary::PosType::Noun as i32),
                    ..Default::default()
                }],
            }],
            ..Default::default()
        };
        std::fs::write(
            profile.path().join("user_dictionary.db"),
            storage.encode_to_vec(),
        )
        .unwrap();
        let before = read(profile.path(), 0).unwrap();
        let mut worker = Mozc::start(&crate::default_worker(), profile.path()).unwrap();
        worker.set_privacy(Privacy::Normal).unwrap();
        let id = worker.create_session().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            worker.reset(id).unwrap();
            for ch in "kanadejisho".chars() {
                worker
                    .key(
                        id,
                        &Key {
                            text: ch.to_string(),
                            ..Default::default()
                        },
                        "",
                    )
                    .unwrap();
            }
            let result = worker
                .key(
                    id,
                    &Key {
                        special: SpecialKey::Space as i32,
                        ..Default::default()
                    },
                    "",
                )
                .unwrap();
            assert!(
                kanade_ipc::reading(&result.preedit) == "かなでじしょ",
                "fixture reading differs"
            );
            if result
                .candidates
                .iter()
                .any(|c| c.value == "KanadeDictionaryFixture")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "dictionary fixture was not loaded"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        worker.reset(id).unwrap();
        worker.set_privacy(Privacy::Private).unwrap();
        for ch in "kanadejisho".chars() {
            worker
                .key(
                    id,
                    &Key {
                        text: ch.to_string(),
                        ..Default::default()
                    },
                    "",
                )
                .unwrap();
        }
        let private = worker
            .key(
                id,
                &Key {
                    special: SpecialKey::Space as i32,
                    ..Default::default()
                },
                "",
            )
            .unwrap();
        assert!(
            !private
                .candidates
                .iter()
                .any(|c| c.value == "KanadeDictionaryFixture")
        );
        worker.reset(id).unwrap();
        worker.sync().unwrap();
        assert_eq!(read(profile.path(), 0).unwrap().revision, before.revision);
    }
    #[test]
    fn pages_cross_dictionary_boundaries_and_revisions_cover_the_original_file() {
        let profile = tempfile::tempdir().unwrap();
        let empty = read(profile.path(), 0).unwrap();
        assert_eq!(empty.total, 0);
        let dictionary = |name: &str, count: usize| pb::UserDictionary {
            name: Some(name.into()),
            entries: (0..count)
                .map(|index| pb::user_dictionary::Entry {
                    key: Some("かなで".into()),
                    value: Some(format!("Fixture{index}")),
                    pos: Some(pb::user_dictionary::PosType::Noun as i32),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let storage = pb::UserDictionaryStorage {
            dictionaries: vec![dictionary("First", 1), dictionary("Second", 50)],
            ..Default::default()
        };
        let path = profile.path().join("user_dictionary.db");
        std::fs::write(&path, storage.encode_to_vec()).unwrap();
        let first = read(profile.path(), 0).unwrap();
        let second = read(profile.path(), 50).unwrap();
        assert_eq!(first.total, 51);
        assert_eq!(first.entries.len(), 50);
        assert_eq!(second.entries.len(), 1);
        assert_ne!(first.revision, empty.revision);
        assert_eq!(first.revision, second.revision);
        assert_eq!(second.entries[0].dictionary_index, 1);
        assert_eq!(second.entries[0].entry_index, 49);
        assert_eq!(second.entries[0].pos.as_deref(), Some("noun"));
        assert!(read(profile.path(), 51).unwrap().entries.is_empty());
        let mut invalid = storage.clone();
        invalid.dictionaries[0].name = Some("x".repeat(301));
        std::fs::write(&path, invalid.encode_to_vec()).unwrap();
        assert!(matches!(read(profile.path(), 0), Err(Error::Invalid)));
        std::fs::write(&path, [0xff]).unwrap();
        assert!(matches!(read(profile.path(), 0), Err(Error::Invalid)));
        File::create(path).unwrap().set_len(MAX_BYTES + 1).unwrap();
        assert!(matches!(read(profile.path(), 0), Err(Error::Invalid)));
    }
}
