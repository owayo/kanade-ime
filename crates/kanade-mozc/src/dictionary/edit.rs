use super::*;
use std::{
    io::Write,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewEntry {
    pub reading: String,
    pub value: String,
    #[serde(default = "noun")]
    pub pos: String,
    #[serde(default)]
    pub comment: String,
}
fn noun() -> String {
    "noun".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Edit {
    Upsert {
        revision: String,
        dictionary_index: Option<u32>,
        entry_index: Option<u32>,
        entry: NewEntry,
    },
    Delete {
        revision: String,
        dictionary_index: u32,
        entry_index: u32,
    },
    CreateDictionary {
        revision: String,
        name: String,
    },
    RenameDictionary {
        revision: String,
        dictionary_index: u32,
        name: String,
    },
    DeleteDictionary {
        revision: String,
        dictionary_index: u32,
    },
}
impl Edit {
    pub fn revision(&self) -> &str {
        match self {
            Self::Upsert { revision, .. }
            | Self::Delete { revision, .. }
            | Self::CreateDictionary { revision, .. }
            | Self::RenameDictionary { revision, .. }
            | Self::DeleteDictionary { revision, .. } => revision,
        }
    }
}

/// Holds the writer lock and an already-synced immutable replacement.
pub struct Prepared {
    target: PathBuf,
    staged: PathBuf,
    revision: String,
    published: bool,
    _lock: WriterLock,
}
struct WriterLock(File);
impl Drop for WriterLock {
    fn drop(&mut self) {
        // A concurrent spawn can inherit the open file description until exec.
        // Explicit unlock releases our lock even while that duplicate is open.
        let _ = self.0.unlock();
    }
}
impl Prepared {
    pub fn revision(&self) -> &str {
        &self.revision
    }
    pub fn published(&self) -> bool {
        self.published
    }
    /// Only call after the engine admits publication. This performs one atomic rename;
    /// parsing, validation, hashing and file synchronization have already completed.
    pub fn publish(&mut self) -> Result<(), Error> {
        if !self.published {
            std::fs::rename(&self.staged, &self.target)?;
            self.published = true;
        }
        Ok(())
    }
    /// Directory durability belongs on the management thread, after publication.
    pub fn sync_directory(&self) -> Result<(), Error> {
        #[cfg(unix)]
        File::open(self.target.parent().ok_or(Error::Invalid)?)?.sync_all()?;
        Ok(())
    }
}
impl Drop for Prepared {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_file(&self.staged);
        }
    }
}

pub fn prepare(profile: &Path, edit: Edit) -> Result<Prepared, Error> {
    std::fs::create_dir_all(profile)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(profile, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut options = File::options();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(profile.join("kanade-dictionary.lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Err(Error::Busy),
        Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
    }
    let lock = WriterLock(lock);
    let (bytes, mut storage) = load(profile)?;
    if edit.revision() != revision(&bytes) {
        return Err(Error::Conflict);
    }
    // prost cannot preserve unknown fields. Never silently discard an extension or
    // an unsupported encoding from an existing dictionary during a known-field edit.
    if storage.version.unwrap_or(0) != 0 || storage.encode_to_vec() != bytes {
        return Err(Error::Unsupported);
    }
    apply(&mut storage, edit)?;
    validate_storage(&storage)?;
    let bytes = storage.encode_to_vec();
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Error::Invalid);
    }
    remove_stale(profile)?;
    let mut options = File::options();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let (staged, mut file) = loop {
        let sequence = SEQUENCE
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::Invalid)?;
        let path = profile.join(format!(
            "kanade-dictionary-{}-{sequence}.next",
            std::process::id()
        ));
        match options.open(&path) {
            Ok(file) => break (path, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    };
    let prepared = Prepared {
        target: profile.join("user_dictionary.db"),
        staged,
        revision: revision(&bytes),
        published: false,
        _lock: lock,
    };
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    Ok(prepared)
}

fn remove_stale(profile: &Path) -> Result<(), Error> {
    for entry in std::fs::read_dir(profile)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name
            .to_str()
            .and_then(|s| s.strip_prefix("kanade-dictionary-"))
            .and_then(|s| s.strip_suffix(".next"))
        else {
            continue;
        };
        if name.split_once('-').is_some_and(|(pid, sequence)| {
            pid.parse::<u32>().is_ok() && sequence.parse::<u64>().is_ok()
        }) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(())
}
fn text(value: &str, required: bool) -> Result<(), Error> {
    if (required && value.trim().is_empty())
        || value.len() > 300
        || value.chars().any(char::is_control)
    {
        return Err(Error::Invalid);
    }
    Ok(())
}
fn value(entry: NewEntry, target: &mut pb::user_dictionary::Entry) -> Result<(), Error> {
    text(&entry.reading, true)?;
    text(&entry.value, true)?;
    text(&entry.comment, false)?;
    if !entry.reading.chars().all(|c| c.is_ascii_graphic()
        || matches!(c, 'ぁ'..='ゖ' | '゙'..='゜' | 'ァ'..='ヺ' | '・' | 'ー' | '、' | '。' | '「'..='』' | '〜' | '！'..='～' | '｡'..='ﾟ')) {
        return Err(Error::Invalid);
    }
    let pos = pb::user_dictionary::PosType::from_str_name(&entry.pos.to_ascii_uppercase())
        .ok_or(Error::Invalid)?;
    if pos == pb::user_dictionary::PosType::NoPos {
        return Err(Error::Invalid);
    }
    target.key = Some(entry.reading);
    target.value = Some(entry.value);
    target.pos = Some(pos as i32);
    target.comment = Some(entry.comment);
    Ok(())
}
fn create(storage: &mut pb::UserDictionaryStorage, name: String) -> Result<usize, Error> {
    text(&name, true)?;
    if storage.dictionaries.len() >= 100 {
        return Err(Error::Invalid);
    }
    let id = storage
        .dictionaries
        .iter()
        .filter_map(|d| d.id)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(Error::Invalid)?;
    storage.dictionaries.push(pb::UserDictionary {
        id: Some(id),
        name: Some(name),
        entries: vec![],
    });
    Ok(storage.dictionaries.len() - 1)
}
fn apply(storage: &mut pb::UserDictionaryStorage, edit: Edit) -> Result<(), Error> {
    match edit {
        Edit::Upsert {
            dictionary_index,
            entry_index,
            entry,
            ..
        } => {
            let index = match dictionary_index {
                Some(index) => index as usize,
                None if entry_index.is_some() => return Err(Error::Invalid),
                None => match storage
                    .dictionaries
                    .iter()
                    .position(|d| d.name.as_deref() == Some("Kanade 手動登録"))
                {
                    Some(index) => index,
                    None => create(storage, "Kanade 手動登録".into())?,
                },
            };
            let dictionary = storage.dictionaries.get_mut(index).ok_or(Error::Invalid)?;
            if let Some(index) = entry_index {
                value(
                    entry,
                    dictionary
                        .entries
                        .get_mut(index as usize)
                        .ok_or(Error::Invalid)?,
                )?;
            } else {
                let mut new = pb::user_dictionary::Entry::default();
                value(entry, &mut new)?;
                if let Some(old) = dictionary
                    .entries
                    .iter_mut()
                    .find(|old| old.key == new.key && old.value == new.value && old.pos == new.pos)
                {
                    old.comment = new.comment;
                } else {
                    dictionary.entries.push(new);
                }
            }
        }
        Edit::Delete {
            dictionary_index,
            entry_index,
            ..
        } => {
            let dictionary = storage
                .dictionaries
                .get_mut(dictionary_index as usize)
                .ok_or(Error::Invalid)?;
            if entry_index as usize >= dictionary.entries.len() {
                return Err(Error::Invalid);
            }
            dictionary.entries.remove(entry_index as usize);
        }
        Edit::CreateDictionary { name, .. } => {
            create(storage, name)?;
        }
        Edit::RenameDictionary {
            dictionary_index,
            name,
            ..
        } => {
            text(&name, true)?;
            storage
                .dictionaries
                .get_mut(dictionary_index as usize)
                .ok_or(Error::Invalid)?
                .name = Some(name);
        }
        Edit::DeleteDictionary {
            dictionary_index, ..
        } => {
            if dictionary_index as usize >= storage.dictionaries.len() {
                return Err(Error::Invalid);
            }
            storage.dictionaries.remove(dictionary_index as usize);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry() -> NewEntry {
        NewEntry {
            reading: "かなで".into(),
            value: "KanadeFixture".into(),
            pos: "noun".into(),
            comment: String::new(),
        }
    }
    fn upsert(revision: String) -> Edit {
        Edit::Upsert {
            revision,
            dictionary_index: None,
            entry_index: None,
            entry: entry(),
        }
    }
    fn commit(profile: &Path, edit: Edit) -> Page {
        let mut prepared = prepare(profile, edit).unwrap();
        prepared.publish().unwrap();
        prepared.sync_directory().unwrap();
        assert!(prepared.published());
        let result = read(profile, 0).unwrap();
        assert_eq!(result.revision, prepared.revision());
        result
    }
    #[test]
    fn cancellation_conflicts_and_writer_lock_leave_the_published_file_unchanged() {
        let profile = tempfile::tempdir().unwrap();
        let before = read(profile.path(), 0).unwrap();
        let pending = prepare(profile.path(), upsert(before.revision.clone())).unwrap();
        assert!(!pending.published());
        assert_eq!(read(profile.path(), 0).unwrap().revision, before.revision);
        assert!(matches!(
            prepare(profile.path(), upsert(before.revision.clone())),
            Err(Error::Busy)
        ));
        drop(pending);
        assert!(!profile.path().join("user_dictionary.db").exists());
        let after = commit(profile.path(), upsert(before.revision.clone()));
        assert_eq!(after.total, 1);
        assert!(matches!(
            prepare(profile.path(), upsert(before.revision)),
            Err(Error::Conflict)
        ));
        assert_eq!(read(profile.path(), 0).unwrap(), after);
        let repeated = commit(profile.path(), upsert(after.revision.clone()));
        assert_eq!(repeated.total, 1);
        assert_eq!(repeated.revision, after.revision);
    }
    #[test]
    fn entry_and_dictionary_edits_preserve_unedited_locale_and_metadata() {
        let profile = tempfile::tempdir().unwrap();
        let initial = commit(
            profile.path(),
            upsert(read(profile.path(), 0).unwrap().revision),
        );
        let (_, mut storage) = load(profile.path()).unwrap();
        storage.dictionaries[0].entries[0].locale = Some("ja".into());
        std::fs::write(
            profile.path().join("user_dictionary.db"),
            storage.encode_to_vec(),
        )
        .unwrap();
        let mut new = entry();
        new.value = "UpdatedFixture".into();
        let updated = commit(
            profile.path(),
            Edit::Upsert {
                revision: read(profile.path(), 0).unwrap().revision,
                dictionary_index: Some(0),
                entry_index: Some(0),
                entry: new,
            },
        );
        assert_eq!(updated.total, initial.total);
        let (_, storage) = load(profile.path()).unwrap();
        assert_eq!(storage.dictionaries[0].id, Some(1));
        assert_eq!(
            storage.dictionaries[0].entries[0].locale.as_deref(),
            Some("ja")
        );
        let created = commit(
            profile.path(),
            Edit::CreateDictionary {
                revision: updated.revision,
                name: "Second".into(),
            },
        );
        assert_eq!(created.dictionaries.len(), 2);
        let renamed = commit(
            profile.path(),
            Edit::RenameDictionary {
                revision: created.revision,
                dictionary_index: 1,
                name: "Renamed".into(),
            },
        );
        assert_eq!(renamed.dictionaries[1].name, "Renamed");
        let deleted = commit(
            profile.path(),
            Edit::Delete {
                revision: renamed.revision,
                dictionary_index: 0,
                entry_index: 0,
            },
        );
        assert_eq!(deleted.total, 0);
        let removed = commit(
            profile.path(),
            Edit::DeleteDictionary {
                revision: deleted.revision,
                dictionary_index: 1,
            },
        );
        assert_eq!(removed.dictionaries.len(), 1);
    }
    #[test]
    fn unsupported_fields_and_invalid_words_are_not_rewritten() {
        let profile = tempfile::tempdir().unwrap();
        let current = commit(
            profile.path(),
            upsert(read(profile.path(), 0).unwrap().revision),
        );
        let path = profile.path().join("user_dictionary.db");
        let original = std::fs::read(&path).unwrap();
        let mut unknown = original.clone();
        unknown.extend_from_slice(&[0xa0, 0x06, 0x01]); // Unknown storage field 100.
        std::fs::write(&path, &unknown).unwrap();
        assert!(matches!(
            prepare(
                profile.path(),
                upsert(read(profile.path(), 0).unwrap().revision)
            ),
            Err(Error::Unsupported)
        ));
        assert!(std::fs::read(&path).unwrap() == unknown);
        std::fs::write(&path, &original).unwrap();
        for (reading, value, pos) in [
            ("漢字", "Fixture", "noun"),
            ("かな", "bad\nvalue", "noun"),
            ("かな", "Fixture", "invalid"),
            ("かな", "Fixture", "no_pos"),
        ] {
            let edit = Edit::Upsert {
                revision: current.revision.clone(),
                dictionary_index: Some(0),
                entry_index: None,
                entry: NewEntry {
                    reading: reading.into(),
                    value: value.into(),
                    pos: pos.into(),
                    comment: String::new(),
                },
            };
            assert!(matches!(prepare(profile.path(), edit), Err(Error::Invalid)));
            assert!(std::fs::read(&path).unwrap() == original);
        }
    }
    #[test]
    #[cfg(unix)]
    fn closing_a_writer_unlocks_even_if_a_spawn_inherited_its_file_description() {
        let profile = tempfile::tempdir().unwrap();
        let edit = upsert(read(profile.path(), 0).unwrap().revision);
        let pending = prepare(profile.path(), edit.clone()).unwrap();
        // dup models the open file description inherited between fork and exec.
        let inherited = pending._lock.0.try_clone().unwrap();
        drop(pending);
        assert!(prepare(profile.path(), edit).is_ok());
        drop(inherited);
    }
}
