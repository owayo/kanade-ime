// SPDX-License-Identifier: MIT
//! Correction evidence is ephemeral; persistent records contain individual terms only.
pub mod service;
use hasami::{Analyzer, ime::TermShape, kana::reading_key};
use kanade_ipc::{Commit, Privacy, Word, utf16_len};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
};

const FIRST_DELETE_MS: u64 = 10_000;
const IDLE_MS: u64 = 15_000;
const TOTAL_MS: u64 = 60_000;
const MAX_RECORDS: usize = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Evidence {
    Strong,
    Weak,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Correction {
    pub reading: String,
    pub accepted: String,
    pub rejected: Option<String>,
    pub evidence: Evidence,
}
#[derive(Clone)]
struct Receipt {
    id: u64,
    context: u64,
    commit: Commit,
    created: u64,
    touched: u64,
}
enum State {
    Idle,
    Watching(Receipt),
    Deleting {
        receipt: Receipt,
        remaining_utf16: u32,
        evidence: Evidence,
    },
    Retyping {
        receipt: Receipt,
        old: Vec<Word>,
        new: Vec<Word>,
        evidence: Evidence,
    },
}
pub struct Detector {
    state: State,
}
impl Default for Detector {
    fn default() -> Self {
        Self { state: State::Idle }
    }
}
impl Detector {
    pub fn cancel(&mut self) {
        self.state = State::Idle;
    }
    /// Call only after the frontend confirms successful document application.
    pub fn confirmed(
        &mut self,
        id: u64,
        context: u64,
        commit: Commit,
        privacy: Privacy,
        now: u64,
    ) -> Vec<Correction> {
        if privacy != Privacy::Normal || commit.transient || !valid_words(&commit) {
            self.cancel();
            return vec![];
        }
        let state = std::mem::replace(&mut self.state, State::Idle);
        let mut corrections = vec![];
        match state {
            State::Retyping {
                mut receipt,
                old,
                mut new,
                evidence,
            } if timely(&receipt, context, now) => {
                new.extend(commit.words.clone());
                receipt.touched = now;
                let target = reading_key(&join_reading(&old));
                let current = reading_key(&join_reading(&new));
                if current == target {
                    corrections = word_diff(&old, &new, evidence);
                } else if target.starts_with(&current) {
                    self.state = State::Retyping {
                        receipt,
                        old,
                        new,
                        evidence,
                    };
                    return vec![];
                }
            }
            State::Deleting {
                mut receipt,
                remaining_utf16,
                evidence,
            } if timely(&receipt, context, now) => {
                if let Some(old) = suffix_words(&receipt.commit.words, remaining_utf16) {
                    receipt.touched = now;
                    let current = reading_key(&commit.reading);
                    let target = reading_key(&join_reading(&old));
                    if current == target {
                        corrections = word_diff(&old, &commit.words, evidence);
                    } else if target.starts_with(&current) {
                        self.state = State::Retyping {
                            receipt,
                            old,
                            new: commit.words,
                            evidence,
                        };
                        return vec![];
                    }
                }
            }
            _ => {}
        }
        self.state = State::Watching(Receipt {
            id,
            context,
            commit,
            created: now,
            touched: now,
        });
        corrections
    }
    /// `remaining_utf16` comes from a verified document range, never key counts.
    /// Weak evidence is permitted only for simple one-scalar kana/kanji deletion.
    pub fn deletion(
        &mut self,
        context: u64,
        remaining_utf16: u32,
        evidence: Evidence,
        now: u64,
    ) -> Option<u64> {
        let state = std::mem::replace(&mut self.state, State::Idle);
        let (mut receipt, previous) = match state {
            State::Watching(r) if now.saturating_sub(r.created) <= FIRST_DELETE_MS => {
                let len = utf16_len(&r.commit.value);
                (r, len)
            }
            State::Deleting {
                receipt,
                remaining_utf16: previous,
                evidence: old,
            } if old == evidence => (receipt, previous),
            _ => return None,
        };
        if !timely(&receipt, context, now) || remaining_utf16 >= previous {
            return None;
        }
        if evidence == Evidence::Weak
            && !receipt
                .commit
                .value
                .chars()
                .all(|c| matches!(c,'ぁ'..='ゖ'|'ァ'..='ヶ'|'一'..='龯'))
        {
            return None;
        }
        receipt.touched = now;
        let undo = receipt.id;
        self.state = State::Deleting {
            receipt,
            remaining_utf16,
            evidence,
        };
        Some(undo)
    }
}
fn timely(receipt: &Receipt, context: u64, now: u64) -> bool {
    context == receipt.context
        && now >= receipt.touched
        && now.saturating_sub(receipt.touched) <= IDLE_MS
        && now.saturating_sub(receipt.created) <= TOTAL_MS
}
fn valid_words(commit: &Commit) -> bool {
    !commit.reading.is_empty()
        && !commit.value.is_empty()
        && commit.value.len() <= 4096
        && !commit.words.is_empty()
        && commit
            .words
            .iter()
            .all(|w| !w.reading.is_empty() && !w.value.is_empty())
        && join_reading(&commit.words) == commit.reading
        && join_value(&commit.words) == commit.value
}
fn join_reading(words: &[Word]) -> String {
    words.iter().map(|w| w.reading.as_str()).collect()
}
fn join_value(words: &[Word]) -> String {
    words.iter().map(|w| w.value.as_str()).collect()
}
fn suffix_words(words: &[Word], remaining: u32) -> Option<Vec<Word>> {
    let mut at = 0;
    for (i, w) in words.iter().enumerate() {
        if at == remaining {
            return Some(words[i..].to_vec());
        }
        at += utf16_len(&w.value);
    }
    None
}
fn word_diff(old: &[Word], new: &[Word], evidence: Evidence) -> Vec<Correction> {
    let mut start = 0;
    while old.get(start).is_some() && old.get(start) == new.get(start) {
        start += 1;
    }
    let (mut a, mut b) = (old.len(), new.len());
    while a > start && b > start && old.get(a - 1) == new.get(b - 1) {
        a -= 1;
        b -= 1;
    }
    let (Some(old), Some(new)) = (old.get(start..a), new.get(start..b)) else {
        return vec![];
    };
    let reading = reading_key(&join_reading(new));
    let accepted = join_value(new);
    let rejected = join_value(old);
    if reading.is_empty() || accepted == rejected || reading != reading_key(&join_reading(old)) {
        return vec![];
    }
    vec![Correction {
        reading,
        accepted,
        rejected: (evidence == Evidence::Strong).then_some(rejected),
        evidence,
    }]
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub id: u64,
    pub correction: Correction,
    pub time: u64,
    pub vocabulary: bool,
    /// Zero for correction evidence; explicit choices are aggregated per term.
    #[serde(default)]
    pub selection_count: u32,
    /// Store-local sequence, independent of wall clock and process lifetime.
    #[serde(default)]
    pub last_selected: u64,
}
#[derive(Clone)]
struct SelectionUndo {
    id: u64,
    version: u64,
    previous: Option<Record>,
}
impl SelectionUndo {
    /// Remove an earlier contribution from a later rollback snapshot too.
    /// Otherwise undoing two selections out of order could restore the first.
    fn forget(&mut self, undone: &Self) {
        if self.id != undone.id {
            return;
        }
        if let Some(previous) = &mut self.previous {
            if previous.last_selected == undone.version {
                self.previous = undone.previous.clone();
            } else if previous.last_selected > undone.version {
                previous.selection_count = previous.selection_count.saturating_sub(1);
            }
        }
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Store {
    records: BTreeMap<u64, Record>,
    next_id: u64,
    #[serde(default)]
    blocked: Vec<String>,
    #[serde(skip)]
    index: BTreeMap<String, ReadingIndex>,
}
#[derive(Clone, Default)]
struct ReadingIndex {
    preferences: Vec<String>,
    vocabulary: Vec<String>,
    rejected: std::collections::BTreeSet<String>,
}
impl Store {
    fn rebuild_index(&mut self) {
        self.index.clear();
        let mut scores = BTreeMap::<String, BTreeMap<String, (u64, u64)>>::new();
        for record in self.records.values().rev() {
            let key = reading_key(&record.correction.reading);
            let entry = self.index.entry(key.clone()).or_default();
            let accepted = &record.correction.accepted;
            let rank = scores
                .entry(key)
                .or_default()
                .entry(accepted.clone())
                .or_default();
            let frequency = if record.selection_count > 0 {
                u64::from(record.selection_count)
            } else {
                1
            };
            rank.0 = rank.0.saturating_add(frequency).min(1_000_000);
            rank.1 = rank.1.max(record.id.max(record.last_selected));
            if record.vocabulary
                && entry.vocabulary.len() < 16
                && !entry.vocabulary.contains(accepted)
            {
                entry.vocabulary.push(accepted.clone());
            }
            if let Some(rejected) = &record.correction.rejected {
                entry.rejected.insert(rejected.clone());
            }
        }
        for (reading, values) in scores {
            let latest = values.values().map(|v| v.1).max().unwrap_or(0);
            let mut values: Vec<_> = values.into_iter().collect();
            // A deliberate new choice beats even a frequent old preference.
            // Other choices use frequency, with recency breaking ties.
            values.sort_by_key(|(_, (frequency, last))| {
                std::cmp::Reverse((
                    frequency + if *last == latest { 1_000_001 } else { 0 },
                    *last,
                ))
            });
            self.index.entry(reading).or_default().preferences =
                values.into_iter().take(32).map(|v| v.0).collect();
        }
    }
    pub fn load(path: &Path) -> io::Result<Self> {
        match fs::read(path) {
            Ok(data) if data.len() <= 4 * 1024 * 1024 => {
                let mut store: Self =
                    serde_json::from_slice(&data).map_err(|_| io::ErrorKind::InvalidData)?;
                if store.records.len() > MAX_RECORDS || store.blocked.len() > 4096 {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                if store.records.values().any(|r| {
                    (r.selection_count == 0) != (r.last_selected == 0)
                        || (r.selection_count > 0
                            && (r.vocabulary
                                || r.correction.rejected.is_some()
                                || r.last_selected > store.next_id
                                || !valid_selection(&r.correction.reading, &r.correction.accepted)))
                }) {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                store.rebuild_index();
                Ok(store)
            }
            Ok(_) => Err(io::ErrorKind::InvalidData.into()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }
    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.records.values()
    }
    pub fn apply(
        &mut self,
        mut correction: Correction,
        time: u64,
        vocabulary: bool,
    ) -> Option<u64> {
        correction.reading = reading_key(&correction.reading);
        if self.records.len() >= MAX_RECORDS
            || self.blocked.contains(&correction.reading)
            || correction.reading.len() > 256
            || correction.accepted.len() > 256
            || correction.accepted.is_empty()
            || correction.accepted.contains(['@', '/', '\\', ':'])
            || correction.accepted.chars().any(char::is_whitespace)
        {
            return None;
        }
        let id = self.next_id.checked_add(1)?;
        self.next_id = id;
        self.records.insert(
            id,
            Record {
                id,
                correction,
                time,
                vocabulary,
                selection_count: 0,
                last_selected: 0,
            },
        );
        if serde_json::to_vec(self).ok()?.len() > 4 * 1024 * 1024 {
            self.records.remove(&id);
            return None;
        }
        self.rebuild_index();
        Some(id)
    }
    fn record_selection(&mut self, word: Word, time: u64) -> Option<SelectionUndo> {
        let reading = reading_key(&word.reading);
        if !valid_selection(&reading, &word.value) || self.blocked.contains(&reading) {
            return None;
        }
        let previous = self
            .records
            .values()
            .find(|record| {
                record.selection_count > 0
                    && record.correction.reading == reading
                    && record.correction.accepted == word.value
            })
            .cloned();
        if previous.is_none() && self.records.len() >= MAX_RECORDS {
            return None;
        }
        let version = self.next_id.checked_add(1)?;
        let id = previous.as_ref().map_or(version, |r| r.id);
        let count = previous
            .as_ref()
            .map_or(1, |r| r.selection_count.saturating_add(1));
        self.next_id = version;
        self.records.insert(
            id,
            Record {
                id,
                correction: Correction {
                    reading,
                    accepted: word.value,
                    rejected: None,
                    evidence: Evidence::Weak,
                },
                time,
                vocabulary: false,
                selection_count: count,
                last_selected: version,
            },
        );
        let undo = SelectionUndo {
            id,
            version,
            previous,
        };
        if serde_json::to_vec(self).ok()?.len() > 4 * 1024 * 1024 {
            self.undo_selection(undo);
            return None;
        }
        self.rebuild_index();
        Some(undo)
    }
    fn undo_selection(&mut self, undo: SelectionUndo) {
        let Some(current) = self.records.get_mut(&undo.id) else {
            return;
        };
        if current.last_selected < undo.version {
            return;
        }
        if current.last_selected > undo.version {
            current.selection_count = current.selection_count.saturating_sub(1);
        } else {
            self.records.remove(&undo.id);
            if let Some(record) = undo.previous {
                self.records.insert(record.id, record);
            }
        }
        self.rebuild_index();
    }
    pub fn undo(&mut self, id: u64) -> bool {
        let removed = self.records.remove(&id).is_some();
        if removed {
            self.rebuild_index();
        }
        removed
    }
    pub fn delete_reading(&mut self, reading: &str) {
        let key = reading_key(reading);
        self.records.retain(|_, r| r.correction.reading != key);
        self.rebuild_index();
    }
    pub fn block_reading(&mut self, reading: &str) {
        self.delete_reading(reading);
        let key = reading_key(reading);
        if self.blocked.len() < 4096 && !self.blocked.contains(&key) {
            self.blocked.push(key);
        }
    }
    pub fn clear(&mut self) {
        self.records.clear();
        self.index.clear();
    }
    pub fn preferred(&self, reading: &str, candidates: &[String]) -> Option<String> {
        let key = reading_key(reading);
        self.index
            .get(&key)?
            .preferences
            .iter()
            .find(|v| candidates.contains(v))
            .cloned()
    }
    pub fn preferences(&self, reading: &str) -> Vec<String> {
        self.index
            .get(&reading_key(reading))
            .map(|entry| entry.preferences.clone())
            .unwrap_or_default()
    }
    pub fn rejects(&self, reading: &str, value: &str) -> bool {
        let key = reading_key(reading);
        self.index
            .get(&key)
            .is_some_and(|entry| entry.rejected.contains(value))
    }
    pub fn vocabulary(&self, reading: &str) -> Vec<String> {
        let key = reading_key(reading);
        self.index
            .get(&key)
            .map(|entry| entry.vocabulary.clone())
            .unwrap_or_default()
    }
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
        fs::create_dir_all(parent)?;
        let temp = path.with_extension("next");
        let mut options = File::options();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            options.mode(0o600);
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let mut file = options.open(&temp)?;
        file.write_all(&serde_json::to_vec(self)?)?;
        file.sync_all()?;
        fs::rename(temp, path)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}
fn valid_selection(reading: &str, value: &str) -> bool {
    !reading.is_empty()
        && reading.len() <= 256
        && reading
            .chars()
            .all(|c| matches!(c, 'ぁ'..='ゖ' | 'ァ'..='ヶ' | 'ー'))
        && !value.is_empty()
        && value.len() <= 256
        && value.chars().count() <= 64
        && value.chars().any(char::is_alphabetic)
        && !value.contains(['@', '/', '\\', ':'])
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}
pub fn classify(analyzer: &mut Analyzer, correction: &Correction) -> Option<bool> {
    let report = analyzer.try_classify_term(&correction.accepted).ok()?;
    match report.shape {
        TermShape::KnownWord => Some(report.tokens.iter().all(|t| t.pos.starts_with("名詞,"))),
        TermShape::KnownCompound => Some(true),
        TermShape::ContainsUnknown if report.covers_input => Some(true),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn selected(value: &str) -> Word {
        Word {
            reading: "こうせい".into(),
            value: value.into(),
        }
    }
    #[test]
    fn explicit_choices_aggregate_frequency_but_the_latest_choice_wins() {
        let mut store = Store::default();
        for _ in 0..4 {
            store.record_selection(selected("校正"), 0).unwrap();
        }
        store.record_selection(selected("更生"), 0).unwrap();
        store.record_selection(selected("構成"), 0).unwrap();
        assert_eq!(store.preferences("こうせい"), ["構成", "校正", "更生"]);
        assert_eq!(store.records().count(), 3);
        assert_eq!(
            store
                .records()
                .find(|r| r.correction.accepted == "校正")
                .unwrap()
                .selection_count,
            4
        );
        assert!(store.vocabulary("こうせい").is_empty());
        let id = store
            .records()
            .find(|r| r.correction.accepted == "構成")
            .unwrap()
            .id;
        store.record_selection(selected("構成"), 0).unwrap();
        assert_eq!(
            store
                .records()
                .find(|r| r.correction.accepted == "構成")
                .unwrap()
                .id,
            id
        );
        assert!(store.undo(id));
        assert_eq!(store.preferences("こうせい"), ["更生", "校正"]);
    }
    #[test]
    fn selection_records_are_bounded_terms_and_never_synthetic_vocabulary() {
        let mut store = Store::default();
        for value in [
            "",
            "1234567890123456",
            "two words",
            "a@example.com",
            "~/secret",
            "https:example",
            "行\n改行",
            "制御\u{1b}",
        ] {
            assert!(store.record_selection(selected(value), 0).is_none());
        }
        assert!(
            store
                .record_selection(
                    Word {
                        reading: "not-kana".into(),
                        value: "単語".into()
                    },
                    0
                )
                .is_none()
        );
        store.block_reading("こうせい");
        assert!(store.record_selection(selected("校正"), 0).is_none());
        assert_eq!(store.records().count(), 0);
    }
    #[test]
    fn old_correction_files_and_new_selection_counts_survive_loading() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("learning.json");
        let mut store = Store::default();
        store
            .apply(
                Correction {
                    reading: "こうせい".into(),
                    accepted: "校正".into(),
                    rejected: None,
                    evidence: Evidence::Strong,
                },
                0,
                true,
            )
            .unwrap();
        let mut old = serde_json::to_value(&store).unwrap();
        for record in old["records"].as_object_mut().unwrap().values_mut() {
            record.as_object_mut().unwrap().remove("selection_count");
            record.as_object_mut().unwrap().remove("last_selected");
        }
        fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        let mut loaded = Store::load(&path).unwrap();
        loaded.record_selection(selected("構成"), 0).unwrap();
        loaded.record_selection(selected("構成"), 0).unwrap();
        loaded.save(&path).unwrap();
        let loaded = Store::load(&path).unwrap();
        assert_eq!(loaded.preferences("ｺｳｾｲ"), ["構成", "校正"]);
        assert_eq!(loaded.vocabulary("こうせい"), ["校正"]);
        assert_eq!(
            loaded
                .records()
                .find(|r| r.selection_count > 0)
                .unwrap()
                .selection_count,
            2
        );
    }
    fn commit(value: &str) -> Commit {
        Commit {
            reading: "こうせい".into(),
            value: value.into(),
            words: vec![Word {
                reading: "こうせい".into(),
                value: value.into(),
            }],
            synthetic: false,
            transient: false,
        }
    }
    #[test]
    fn correction_requires_confirmed_deletion_and_same_reading() {
        let mut d = Detector::default();
        assert!(
            d.confirmed(1, 1, commit("構成"), Privacy::Normal, 0)
                .is_empty()
        );
        assert_eq!(d.deletion(1, 0, Evidence::Strong, 1), Some(1));
        let learned = d.confirmed(2, 1, commit("校正"), Privacy::Normal, 2);
        assert_eq!(learned.len(), 1);
        assert_eq!(learned[0].rejected.as_deref(), Some("構成"));
    }
    #[test]
    fn transient_commits_are_never_correction_evidence() {
        for (old_transient, new_transient) in [(true, false), (false, true), (true, true)] {
            let mut detector = Detector::default();
            let mut old = commit("構成");
            old.transient = old_transient;
            detector.confirmed(1, 1, old, Privacy::Normal, 0);
            detector.deletion(1, 0, Evidence::Strong, 1);
            let mut new = commit("校正");
            new.transient = new_transient;
            assert!(detector.confirmed(2, 1, new, Privacy::Normal, 2).is_empty());
        }
    }
    #[test]
    fn partial_word_focus_timeouts_and_private_input_do_not_learn() {
        for (context, left, now, privacy) in [
            (2, 0, 2, Privacy::Normal),
            (1, 1, 2, Privacy::Normal),
            (1, 0, 61_000, Privacy::Normal),
            (1, 0, 2, Privacy::Private),
        ] {
            let mut d = Detector::default();
            d.confirmed(1, 1, commit("構成"), Privacy::Normal, 0);
            d.deletion(context, left, Evidence::Strong, 1);
            assert!(
                d.confirmed(2, context, commit("校正"), privacy, now)
                    .is_empty()
            );
        }
    }
    #[test]
    fn weak_evidence_never_rejects_previous_surface() {
        let mut d = Detector::default();
        d.confirmed(1, 1, commit("構成"), Privacy::Normal, 0);
        d.deletion(1, 0, Evidence::Weak, 1);
        assert!(
            d.confirmed(2, 1, commit("校正"), Privacy::Normal, 2)[0]
                .rejected
                .is_none()
        );
    }
    #[test]
    fn split_commits_are_accumulated_until_reading_matches() {
        let mut d = Detector::default();
        d.confirmed(1, 1, commit("構成"), Privacy::Normal, 0);
        d.deletion(1, 0, Evidence::Strong, 1);
        let mut a = commit("校");
        a.reading = "こう".into();
        a.words[0].reading = "こう".into();
        let mut b = commit("正");
        b.reading = "せい".into();
        b.words[0].reading = "せい".into();
        assert!(d.confirmed(2, 1, a, Privacy::Normal, 2).is_empty());
        assert_eq!(d.confirmed(3, 1, b, Privacy::Normal, 3).len(), 1);
    }
    #[test]
    fn undo_restores_preference_and_vocabulary() {
        let mut s = Store::default();
        let id = s
            .apply(
                Correction {
                    reading: reading_key("こうせい"),
                    accepted: "校正".into(),
                    rejected: Some("構成".into()),
                    evidence: Evidence::Strong,
                },
                0,
                true,
            )
            .unwrap();
        assert_eq!(s.preferred("ｺｳｾｲ", &["校正".into()]), Some("校正".into()));
        assert!(s.undo(id));
        assert!(s.vocabulary("こうせい").is_empty());
        assert!(!s.rejects("こうせい", "構成"));
    }
}
