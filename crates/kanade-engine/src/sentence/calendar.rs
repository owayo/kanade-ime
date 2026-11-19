// SPDX-License-Identifier: MIT
//! Calendar years whose spoken number crosses conversion segments.
use super::Segment;

const DIGITS: &[(&str, u16)] = &[
    ("いち", 1),
    ("に", 2),
    ("さん", 3),
    ("よん", 4),
    ("し", 4),
    ("ご", 5),
    ("ろく", 6),
    ("なな", 7),
    ("しち", 7),
    ("はち", 8),
    ("きゅう", 9),
    ("く", 9),
];
const HUNDREDS: &[(&str, u16)] = &[
    ("ひゃく", 100),
    ("にひゃく", 200),
    ("さんびゃく", 300),
    ("よんひゃく", 400),
    ("ごひゃく", 500),
    ("ろっぴゃく", 600),
    ("ななひゃく", 700),
    ("はっぴゃく", 800),
    ("きゅうひゃく", 900),
];

fn number(root: &str) -> Option<u16> {
    let (mut rest, mut year) = root
        .strip_prefix("にせん")
        .map(|s| (s, 2000))
        .or_else(|| root.strip_prefix("せん").map(|s| (s, 1000)))?;
    if let Some((tail, h)) = HUNDREDS
        .iter()
        .find_map(|(k, h)| rest.strip_prefix(k).map(|tail| (tail, h)))
    {
        year += h;
        rest = tail;
    }
    if let Some((tens, tail)) = rest.split_once("じゅう") {
        let n = if tens.is_empty() {
            1
        } else {
            DIGITS.iter().find(|(k, n)| *k == tens && *n > 1)?.1
        };
        year += 10 * n;
        rest = tail;
    }
    let unit = match rest {
        "" => 0,
        "よ" => 4,
        v => DIGITS.iter().find(|(k, _)| *k == v)?.1,
    };
    Some(year + unit)
}

fn calendar_year(phrase: &str) -> Option<(u16, &str)> {
    let (root, suffix) = phrase.split_once("ねん")?;
    let year = number(root)?;
    let ending = match suffix {
        "だい" => "代",
        "ど" => "度",
        "" | "に" | "の" | "から" | "まで" | "で" | "は" | "には" | "も" if year % 1000 != 0 => {
            suffix
        }
        _ => return None,
    };
    Some((year, ending))
}

fn is_quantity(segment: &Segment) -> bool {
    matches!(
        segment.reading.as_str(),
        "すう" | "なん" | "やく" | "およそ" | "じゅう" | "ひゃく" | "せん" | "まん" | "おく"
    ) || segment.value.ends_with(['万', '億', '兆'])
}

pub(super) fn surface(parts: &[Segment], context: &str) -> Option<String> {
    if parts.len() > 16 {
        return None;
    }
    let mut result = String::new();
    let mut changed = false;
    let mut start = 0;
    while start < parts.len() {
        let mut best = None;
        let mut joined = String::new();
        for (offset, part) in parts[start..].iter().take(8).enumerate() {
            joined.push_str(&part.reading);
            if !joined.starts_with(['に', 'せ']) {
                break;
            }
            if let Some((year, ending)) = calendar_year(&joined) {
                if start > 0
                    && joined.starts_with("せん")
                    && DIGITS.iter().any(|(k, _)| *k == parts[start - 1].reading)
                {
                    continue;
                }
                best = Some((
                    start + offset + 1,
                    format!("{year}年{ending}"),
                    ending.is_empty(),
                ));
            }
        }
        if let Some((end, replacement, bare)) = best {
            if (start > 0 && is_quantity(&parts[start - 1]))
                || (start == 0
                    && ["およそ", "凡そ", "約", "数", "何"]
                        .iter()
                        .any(|s| context.ends_with(s)))
                || (bare
                    && parts
                        .get(end)
                        .is_some_and(|s| s.reading.starts_with("かん")))
            {
                return None;
            }
            let original: String = parts[start..end].iter().map(|s| s.value.as_str()).collect();
            if original.chars().any(|c| !c.is_alphanumeric() && c != 'ー') {
                return None;
            }
            changed |= original != replacement;
            result.push_str(&replacement);
            start = end;
        } else {
            result.push_str(&parts[start].value);
            start += 1;
        }
    }
    changed.then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parts(words: &[(&str, &str)]) -> Vec<Segment> {
        words
            .iter()
            .map(|(reading, value)| Segment {
                reading: reading.to_string(),
                value: value.to_string(),
                ..Default::default()
            })
            .collect()
    }
    #[test]
    fn years_preserve_dates_and_reject_durations() {
        let s = parts(&[
            ("にせん", "二千"),
            ("きゅうねんの", "九年の"),
            ("はる", "春"),
        ]);
        assert_eq!(surface(&s, "当時は").as_deref(), Some("2009年の春"));
        assert!(surface(&s, "およそ").is_none());
        assert!(
            surface(
                &parts(&[("にせん", "二千"), ("きゅうねん", "九年"), ("かん", "間")]),
                ""
            )
            .is_none()
        );
        assert!(surface(&parts(&[("にせんねん", "二千年")]), "").is_none());
        assert!(surface(&parts(&[("にせん", "二、千"), ("きゅうねん", "九年")]), "").is_none());
        assert!(
            surface(
                &parts(&[
                    ("いちまん", "一万"),
                    ("にせん", "二千"),
                    ("きゅうねん", "九年")
                ]),
                ""
            )
            .is_none()
        );
        assert_eq!(number("せんきゅうひゃくはちじゅうよ"), Some(1984));
        assert!(number("にせんさんさん").is_none());
    }
}
