//! Immutable, exact-match emoji readings. Network access and morphology belong
//! only to the explicit data-generation task, never to a conversion request.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::OnceLock};

pub const LIMIT: usize = 32;
pub const MAX_KEY_BYTES: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hit {
    pub emoji: u16,
    pub sources: u16,
}

#[derive(Serialize, Deserialize)]
pub struct Data {
    pub format: u8,
    pub unicode: String,
    pub entries: Vec<String>,
    pub readings: BTreeMap<String, Vec<Hit>>,
    pub sources: BTreeMap<String, String>,
    pub dictionary_sha256: String,
}

/// Normalize lookup text only. Emoji output must never pass through this function.
pub fn normalize(text: &str) -> String {
    if text.len() > MAX_KEY_BYTES {
        return String::new();
    }
    let text = text.trim();
    let text = if text.len() > 2 && text.starts_with(':') && text.ends_with(':') {
        &text[1..text.len() - 1]
    } else {
        text
    };
    hasami::kana::reading_key(text)
        .replace('_', " ")
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Source spellings with text/emoji variation selectors identify the same emoji.
/// ZWJ, skin modifiers, regional indicators and tag sequences remain significant.
pub fn identity(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, '\u{fe0e}' | '\u{fe0f}'))
        .collect()
}

pub struct Catalog(Data);

impl Catalog {
    /// Call during engine initialization, outside synchronous key processing.
    pub fn bundled() -> &'static Self {
        static DATA: OnceLock<Catalog> = OnceLock::new();
        DATA.get_or_init(|| {
            Catalog(
                serde_json::from_slice(include_bytes!("../data/catalog.json"))
                    .expect("bundled emoji catalogue"),
            )
        })
    }

    pub fn lookup(&self, text: &str) -> Vec<&str> {
        self.0
            .readings
            .get(&normalize(text))
            .into_iter()
            .flatten()
            .take(LIMIT)
            .filter_map(|hit| self.0.entries.get(hit.emoji as usize).map(String::as_str))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_preserve_shortcodes_signs_and_kana_distinctions() {
        assert_eq!(normalize(" :THUMBS_UP: "), "thumbs up");
        assert_eq!(normalize("か\u{3099}っこう"), normalize("ｶﾞｯｺｳ"));
        assert_ne!(normalize("+1"), normalize("-1"));
        assert_eq!(normalize(":)"), ":)");
        assert_ne!(normalize("こー"), normalize("こう"));
        assert!(normalize(&"a".repeat(MAX_KEY_BYTES + 1)).is_empty());
    }

    #[test]
    fn bundled_readings_keep_qualified_sequences_and_colloquial_aliases() {
        let data = Catalog::bundled();
        for (key, expected) in [
            ("いいね", "👍"),
            ("ロケット", "🚀"),
            ("赤いハート", "❤️"),
            (":thumbsup:", "👍"),
            ("+1", "👍"),
            ("-1", "👎"),
        ] {
            assert!(
                data.lookup(key).contains(&expected),
                "missing fixture alias"
            );
        }
        assert_eq!(identity("❤"), identity("❤️"));
        assert_ne!(identity("♥"), identity("❤️"));
        for value in ["1️⃣", "🏳️‍🌈", "🇯🇵", "👍🏽", "👨‍👩‍👧‍👦"]
        {
            assert!(data.0.entries.iter().any(|e| e == value));
        }
        assert!(!data.lookup("いいね").iter().any(|s| s.contains('🏽')));
        assert!(data.lookup("存在しない絵文字の読み").is_empty());
        assert!(data.0.readings.values().all(|hits| hits.len() <= LIMIT));
    }
}
