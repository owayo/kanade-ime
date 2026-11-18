use std::{collections::BTreeMap, sync::OnceLock};

const DATA: &str = include_str!("../data/technical-readings.tsv");

pub(super) fn lookup(word: &str) -> Option<&'static str> {
    static WORDS: OnceLock<BTreeMap<&'static str, &'static str>> = OnceLock::new();
    let words = WORDS.get_or_init(|| {
        DATA.lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| line.split_once('\t').expect("invalid built-in reading"))
            .collect()
    });
    words.get(word.to_ascii_lowercase().as_str()).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn built_in_readings_are_unique_and_safe_terms() {
        let mut seen = std::collections::BTreeSet::new();
        for line in DATA
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
        {
            let (word, reading) = line.split_once('\t').unwrap();
            assert!(seen.insert(word));
            assert!(
                !word.is_empty()
                    && word
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            );
            assert!(crate::valid_term(&crate::Term {
                value: word.into(),
                reading: reading.into()
            }));
        }
        assert!(!seen.is_empty());
    }
}
