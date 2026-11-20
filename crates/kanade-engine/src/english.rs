// SPDX-License-Identifier: MIT
use kanade_ipc::{Key, MOD_SHIFT, SpecialKey};
use kanade_mozc::Snapshot;
use std::{collections::BTreeMap, sync::OnceLock};

pub(super) struct Spellings {
    readings: BTreeMap<&'static str, &'static str>,
    latin: BTreeMap<String, &'static str>,
}

impl Spellings {
    pub(super) fn bundled() -> &'static Self {
        static DATA: OnceLock<Spellings> = OnceLock::new();
        DATA.get_or_init(|| {
            let readings: BTreeMap<_, _> = include_str!("../data/english-spellings.tsv")
                .lines()
                .map(|line| line.split_once('\t').expect("invalid spelling entry"))
                .collect();
            let mut latin = BTreeMap::new();
            for word in readings.values().flat_map(|row| row.split('\t')) {
                let key = word.to_ascii_lowercase();
                // Prefer an attested capitalized form for a literal typing
                // (e.g. "Monday" even if another reading includes "monday").
                if word != key {
                    latin.insert(key, word);
                } else {
                    latin.entry(key).or_insert(word);
                }
            }
            Self { readings, latin }
        })
    }

    pub(super) fn lookup(&self, reading: &str) -> Option<&'static str> {
        if reading.len() > 48 * 6 {
            return None;
        }
        self.readings
            .get(hasami::kana::reading_key(reading).as_str())
            .copied()
    }

    pub(super) fn literal(&self, spelling: &str) -> Option<&'static str> {
        self.latin.get(spelling).copied()
    }
}

/// Only uninterrupted ASCII typing at the end of the current composition.
/// A kana edit can delete several romaji keys at once, so discard the hint
/// when the backend edits the preedit rather than guessing the remaining keys.
pub(super) fn update_literal(
    spelling: &mut Option<String>,
    before: &Snapshot,
    after: &Snapshot,
    key: &Key,
) {
    if after.preedit.segments.is_empty() || !after.consumed {
        *spelling = None;
        return;
    }
    if after.converting {
        return;
    }
    if key.special != SpecialKey::None as i32
        || key.modifiers & !MOD_SHIFT != 0
        || key.text.is_empty()
        || !key.text.bytes().all(|b| b.is_ascii_alphabetic())
    {
        *spelling = None;
        return;
    }
    if before.preedit.segments.is_empty() || after.commit.is_some() {
        *spelling = Some(String::new());
    }
    if let Some(word) = spelling {
        if word.len() + key.text.len() <= 64 {
            word.extend(key.text.bytes().map(|b| char::from(b.to_ascii_lowercase())));
        } else {
            *spelling = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_readings_keep_case_and_distinct_homophones() {
        let data = Spellings::bundled();
        for key in ["まんでー", "マンデー", "ﾏﾝﾃﾞｰ", "まんでい"] {
            assert_eq!(data.lookup(key).unwrap().split('\t').next(), Some("Monday"));
        }
        for (key, expected) in [
            ("さんでー", "Sunday"),
            ("せぷてんばー", "September"),
            ("こーひー", "coffee"),
            ("ぐーぐる", "Google"),
            ("らいと", "light"),
            ("らいと", "right"),
        ] {
            assert!(data.lookup(key).unwrap().split('\t').any(|w| w == expected));
        }
        assert!(data.lookup("まんでーにいく").is_none());
    }
}
