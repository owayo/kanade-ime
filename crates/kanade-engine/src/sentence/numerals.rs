// SPDX-License-Identifier: MIT
//! Numbers written with ASCII digits instead of kanji numerals or wide digits.
//!
//! The reading of 二位 and 2位 is the same, so the spelling follows the style
//! of the text. This module only offers the alternatives; the model compares
//! them after the committed context.

const DIGITS: &str = "〇一二三四五六七八九";
/// Words counted with a preceding number.
const COUNTERS: &str = "年月日時分秒人回度番位号本枚個台冊巻話作曲歳才世代階点種部社校名着勝敗戦区週周倍割件組両編章節条項期次等級段票店駅線面塁打球機隻門発頭匹羽足棟軒室席床基体行列桁歩例類品弾版刷団艦連隊万億兆丁";
/// Words that begin with a numeral and a counter but are not written with digits.
const WORDS: &[&str] = &[
    "十分",
    "千代",
    "一体",
    "一連",
    "一段",
    "一族",
    "一門",
    "一団",
    "一線",
    "一発",
    "一歩",
    "一足",
    "一例",
    "一時的",
    "一時期",
    "一種の",
];

/// A number in a text and its spelling with ASCII digits.
struct Run {
    start: usize,
    end: usize,
    digits: String,
}

fn unit(c: char) -> Option<u32> {
    match c {
        '十' => Some(10),
        '百' => Some(100),
        '千' => Some(1000),
        _ => None,
    }
}

pub(super) fn numeral(c: char) -> bool {
    DIGITS.contains(c) || unit(c).is_some()
}

fn wide(c: char) -> bool {
    ('０'..='９').contains(&c)
}

// 二十三 is 23 and 一九九〇 is 1990; 二十百 and 三三十 are not numbers.
pub(super) fn value(word: &str) -> Option<String> {
    let digit = |c: char| DIGITS.chars().position(|d| d == c);
    if word.chars().all(|c| digit(c).is_some()) {
        return Some(
            word.chars()
                .filter_map(digit)
                .map(|d| char::from(b'0' + d as u8))
                .collect(),
        );
    }
    let mut total = 0u32;
    let mut pending = None;
    for c in word.chars() {
        if let Some(d) = digit(c) {
            if pending.is_some() || d == 0 {
                return None;
            }
            pending = Some(d as u32);
        } else {
            let u = unit(c)?;
            // Units must decrease: 千, then 百, then 十.
            if !total.is_multiple_of(u * 10) {
                return None;
            }
            total += pending.take().unwrap_or(1) * u;
        }
    }
    Some((total + pending.unwrap_or(0)).to_string())
}

fn counted(rest: &str) -> bool {
    let mut chars = rest.chars();
    match chars.next() {
        Some('つ') => true,
        Some('か' | 'ヶ' | 'カ' | 'ヵ' | '箇') => {
            matches!(chars.next(), Some('月' | '所' | '国' | '年' | '条' | '村'))
        }
        Some('ノ' | 'ツ' | 'ケ') => false,
        Some(c) => COUNTERS.contains(c) || matches!(c, 'ァ'..='ヺ'),
        None => false,
    }
}

fn runs(text: &str, following: &str, spoken: &[String]) -> Vec<Run> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut found = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (start, c) = chars[i];
        let member: fn(char) -> bool = if wide(c) {
            wide
        } else if numeral(c) {
            numeral
        } else {
            i += 1;
            continue;
        };
        let mut j = i;
        while j < chars.len() && member(chars[j].1) {
            j += 1;
        }
        let end = chars.get(j).map_or(text.len(), |x| x.0);
        let word = &text[start..end];
        i = j;
        if wide(c) {
            let digits = word
                .chars()
                .map(|c| char::from(b'0' + (u32::from(c) - u32::from('０')) as u8))
                .collect();
            found.push(Run { start, end, digits });
            continue;
        }
        let rest = format!("{}{following}", &text[end..]);
        let before = text[..start].chars().next_back();
        let fixed = WORDS.iter().any(|w| {
            w.strip_prefix(word)
                .is_some_and(|tail| rest.starts_with(tail))
        });
        // A number read from the kana is one whatever follows it.
        let known = spoken.iter().any(|number| number == word);
        let amount = !matches!(before, Some('数' | '何' | '幾'))
            && (before == Some('第') || !fixed)
            && counted(&rest);
        if (known || amount)
            && let Some(digits) = value(word)
        {
            found.push(Run { start, end, digits });
        }
    }
    found
}

fn rewrite(text: &str, runs: &[&Run]) -> String {
    let mut result = String::with_capacity(text.len());
    let mut at = 0;
    for run in runs {
        result.push_str(&text[at..run.start]);
        result.push_str(&run.digits);
        at = run.end;
    }
    result.push_str(&text[at..]);
    result
}

/// Spellings of `text` with digits: every number at once, then each of the
/// first two numbers alone when there are several. `following` is the text
/// displayed after it, which decides whether a final numeral is counted.
/// `spoken` are kanji numerals known to be numbers from their reading.
pub(super) fn variants(text: &str, following: &str, spoken: &[String]) -> Vec<String> {
    let found = runs(text, following, spoken);
    let mut result = Vec::new();
    if found.is_empty() {
        return result;
    }
    result.push(rewrite(text, &found.iter().collect::<Vec<_>>()));
    if found.len() > 1 {
        for run in found.iter().take(2) {
            result.push(rewrite(text, &[run]));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counted_numbers_are_offered_with_digits() {
        for (text, expected) in [
            ("同じく二位だった場合には", "同じく2位だった場合には"),
            ("約五年ぶりに", "約5年ぶりに"),
            ("勝ち上がった八チームと", "勝ち上がった8チームと"),
            ("圧倒的一番人気に", "圧倒的1番人気に"),
            ("第二十三回の", "第23回の"),
            ("一九九〇年に", "1990年に"),
            ("千九百六十九年より", "1969年より"),
            ("三百五十号の", "350号の"),
            ("４つの色ボタン", "4つの色ボタン"),
            ("同年６月３日発売の", "同年6月3日発売の"),
            ("一六シーズン優勝", "16シーズン優勝"),
            ("十人の", "10人の"),
            ("第一号に", "第1号に"),
            ("三か月の", "3か月の"),
            ("島式ホーム一面二線の", "島式ホーム1面2線の"),
        ] {
            assert_eq!(variants(text, "", &[])[0], expected, "{text}");
        }
        // Each number can also change alone.
        assert_eq!(
            variants("三人体制での二枚目と", "", &[]),
            [
                "3人体制での2枚目と",
                "3人体制での二枚目と",
                "三人体制での2枚目と"
            ]
        );
        // The counter may be displayed after the scored text.
        assert_eq!(variants("同じく二", "位だった", &[]), ["同じく2"]);
        assert!(variants("同じく二", "", &[]).is_empty());
        // A number known from its reading needs no counter after it.
        let spoken = ["二〇〇".to_owned(), "二五".to_owned()];
        assert_eq!(
            variants("リーグ戦二〇〇試合近くに", "", &spoken),
            ["リーグ戦200試合近くに"]
        );
        assert_eq!(variants("樹高約二五", "", &spoken), ["樹高約25"]);
        assert!(variants("樹高約二五", "", &[]).is_empty());
        assert!(variants("三二五試合", "", &spoken).is_empty());
    }

    #[test]
    fn words_and_vague_amounts_keep_their_kanji() {
        for text in [
            "一般的な",
            "唯一の",
            "一時的に",
            "数十人の",
            "何百年も",
            "一方で",
            "三角形の",
            "十分な時間",
            "一ノ瀬さん",
            "八ヶ岳の",
            "二十百年",
            "統一された",
            "一種の",
            "一体となって",
        ] {
            assert!(variants(text, "", &[]).is_empty(), "{text}");
        }
        assert!(variants("十", "分な時間", &[]).is_empty());
        // An ordinal is a number even where the bare word is not.
        assert_eq!(variants("第一線で", "", &[]), ["第1線で"]);
        assert_eq!(value("二千二十四").as_deref(), Some("2024"));
        assert_eq!(value("千").as_deref(), Some("1000"));
        assert!(value("三三十").is_none());
    }
}
