// SPDX-License-Identifier: MIT
//! Numbers that the converter read as separate words.
//!
//! せんきゅうひゃくろくじゅうきゅう can come back as four segments, none of them a
//! number. This module finds two or more numeral words in the kana of
//! consecutive segments and cuts that range again with the number as one
//! segment, written with kanji numerals and with ASCII digits. It only offers
//! the cut; the model compares it with the displayed text.
use super::{FocusCandidate, Segment, numerals};
use std::ops::Range;

/// Readings of one to nine and their kanji numerals.
const DIGITS: [(&str, char); 9] = [
    ("いち", '一'),
    ("に", '二'),
    ("さん", '三'),
    ("よん", '四'),
    ("ご", '五'),
    ("ろく", '六'),
    ("なな", '七'),
    ("はち", '八'),
    ("きゅう", '九'),
];
const ZERO: (&str, char) = ("ぜろ", '〇');

/// How 千, 百 or 十 is spoken alone and after each multiplier: the kana of the
/// multiplier, the kana of the unit and the multiplier's numeral.
struct Unit {
    mark: char,
    alone: &'static str,
    multiples: &'static [(&'static str, &'static str, char)],
}
const UNITS: [Unit; 3] = [
    Unit {
        mark: '千',
        alone: "せん",
        multiples: &[
            ("に", "せん", '二'),
            ("さん", "ぜん", '三'),
            ("よん", "せん", '四'),
            ("ご", "せん", '五'),
            ("ろく", "せん", '六'),
            ("なな", "せん", '七'),
            ("はっ", "せん", '八'),
            ("きゅう", "せん", '九'),
        ],
    },
    Unit {
        mark: '百',
        alone: "ひゃく",
        multiples: &[
            ("に", "ひゃく", '二'),
            ("さん", "びゃく", '三'),
            ("よん", "ひゃく", '四'),
            ("ご", "ひゃく", '五'),
            ("ろっ", "ぴゃく", '六'),
            ("なな", "ひゃく", '七'),
            ("はっ", "ぴゃく", '八'),
            ("きゅう", "ひゃく", '九'),
        ],
    },
    Unit {
        mark: '十',
        alone: "じゅう",
        multiples: &[
            ("に", "じゅう", '二'),
            ("さん", "じゅう", '三'),
            ("よん", "じゅう", '四'),
            ("ご", "じゅう", '五'),
            ("ろく", "じゅう", '六'),
            ("なな", "じゅう", '七'),
            ("しち", "じゅう", '七'),
            ("はち", "じゅう", '八'),
            ("きゅう", "じゅう", '九'),
        ],
    },
];
/// Kana that continue a syllable or a word and never begin one.
const CONTINUING: &str = "んゃゅょっぁぃぅぇぉゎー";

/// A number at one place in a reading: the byte offset at which each numeral
/// word ends, and its kanji numeral.
struct Parse {
    words: Vec<(usize, char)>,
    /// With 千, 百 or 十, as opposed to a row of digits.
    positional: bool,
}

impl Parse {
    fn end(&self) -> usize {
        self.words.last().map_or(0, |word| word.0)
    }

    fn kanji(&self, words: Range<usize>) -> String {
        self.words[words].iter().map(|word| word.1).collect()
    }

    // Ways to write some of the words: 二十, 20 and ２０.
    fn spellings(&self, words: Range<usize>) -> Vec<String> {
        let kanji = self.kanji(words);
        let digits = numerals::value(&kanji);
        let wide = digits.as_ref().map(|digits| {
            digits
                .bytes()
                .filter_map(|b| char::from_u32(u32::from('０') + u32::from(b - b'0')))
                .collect()
        });
        [Some(kanji), digits, wide].into_iter().flatten().collect()
    }
}

// Every number of two or more numeral words that starts at `at`, the longest
// first. にじゅうに is 22 and, without its last word, 20; はちいち is 81.
fn parses(text: &str, at: usize) -> Vec<Parse> {
    let mut found = Vec::new();
    let mut words = Vec::new();
    let mut pos = at;
    for unit in &UNITS {
        let rest = &text[pos..];
        let multiple = unit.multiples.iter().find(|(multiplier, spoken, _)| {
            rest.strip_prefix(multiplier)
                .is_some_and(|rest| rest.starts_with(spoken))
        });
        if let Some((multiplier, spoken, numeral)) = multiple {
            pos += multiplier.len();
            words.push((pos, *numeral));
            pos += spoken.len();
        } else if rest.starts_with(unit.alone) {
            pos += unit.alone.len();
        } else {
            continue;
        }
        words.push((pos, unit.mark));
        if words.len() >= 2 {
            found.push(Parse {
                words: words.clone(),
                positional: true,
            });
        }
    }
    if !words.is_empty() {
        let rest = &text[pos..];
        let digit = DIGITS
            .iter()
            .chain([&("しち", '七')])
            .find(|(word, _)| rest.starts_with(word))
            .map(|(word, numeral)| (word.len(), *numeral))
            // Four is よ before 年.
            .or_else(|| rest.starts_with("よねん").then_some(("よ".len(), '四')));
        if let Some((len, numeral)) = digit {
            words.push((pos + len, numeral));
            found.push(Parse {
                words,
                positional: true,
            });
        }
    }
    let mut words = Vec::new();
    let mut pos = at;
    while let Some((word, numeral)) = DIGITS
        .iter()
        .chain([&ZERO])
        .find(|(word, _)| text[pos..].starts_with(word))
    {
        // A row of digits that begins with zero is not a number in a sentence.
        if words.is_empty() && *numeral == ZERO.1 {
            break;
        }
        pos += word.len();
        words.push((pos, *numeral));
        if words.len() >= 2 {
            found.push(Parse {
                words: words.clone(),
                positional: false,
            });
        }
    }
    found.sort_by_key(|parse| std::cmp::Reverse(parse.end()));
    found
}

fn written_as_number(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| numerals::numeral(c) || c.is_ascii_digit() || ('０'..='９').contains(&c))
}

fn choice(value: &str) -> FocusCandidate {
    FocusCandidate {
        reading: String::new(),
        annotation: String::new(),
        value: value.to_owned(),
        transliteration: false,
        history_deletable: false,
    }
}

/// A number read across the segments `range`, and how the kana of those
/// segments are cut around it.
#[derive(Clone)]
pub(super) struct Number {
    pub(super) range: Range<usize>,
    reading: String,
    /// Kanji numerals, then ASCII digits.
    spellings: [String; 2],
    /// The kana of the first segment before the number, and of the last
    /// segment after it, as they are displayed.
    left: Option<Segment>,
    right: Option<Segment>,
}

impl Number {
    /// The number with kanji numerals and with ASCII digits.
    pub(super) fn spellings(&self) -> &[String; 2] {
        &self.spellings
    }

    /// The text of the segments with the number written as `spelling`.
    pub(super) fn written(&self, spelling: &str) -> String {
        let left = self.left.as_ref().map_or("", |s| s.value.as_str());
        let right = self.right.as_ref().map_or("", |s| s.value.as_str());
        format!("{left}{spelling}{right}")
    }

    /// The segments that take the place of `range` once `spelling` is chosen.
    /// Each has one spelling: the notation of a number is compared separately.
    pub(super) fn cut(&self, spelling: &str) -> (Vec<Segment>, Vec<Vec<FocusCandidate>>) {
        let number = Segment {
            reading: self.reading.clone(),
            value: spelling.to_owned(),
            ..Default::default()
        };
        let parts: Vec<Segment> = self
            .left
            .iter()
            .cloned()
            .chain([number])
            .chain(self.right.iter().cloned())
            .collect();
        let lists = parts.iter().map(|s| vec![choice(&s.value)]).collect();
        (parts, lists)
    }
}

enum Refusal {
    /// The last numeral word belongs to the text after the number.
    Shorter,
    /// No number starts at this place.
    Whole,
}

struct Reading<'a> {
    text: String,
    /// Byte range of each segment's reading in `text`.
    bounds: Vec<Range<usize>>,
    parts: &'a [Segment],
    lists: &'a [Vec<FocusCandidate>],
}

impl Reading<'_> {
    // Decide whether the number read at `start` may be offered, and cut its
    // segments around it. Also returns where the search goes on.
    fn fit(&self, start: usize, parse: &Parse) -> Result<(Number, usize), Refusal> {
        let (text, bounds, parts) = (&self.text, &self.bounds, self.parts);
        let end = parse.end();
        // The kana after a number begin a word: じゅうに and ん are not 十二.
        if text[end..].starts_with(|c| CONTINUING.contains(c)) {
            return Err(Refusal::Shorter);
        }
        let first = bounds.iter().position(|b| b.contains(&start));
        let last = bounds.iter().position(|b| b.start < end && end <= b.end);
        let (Some(a), Some(b)) = (first, last) else {
            return Err(Refusal::Whole);
        };
        // A number inside one segment is the converter's own word and has its
        // candidates. Without a segment that is all number, the kana around
        // two words only look like one: ひんぱんに, then せんしゅを.
        let whole = |index: usize| start <= bounds[index].start && bounds[index].end <= end;
        if a == b || !(a..=b).any(whole) || text[..start].ends_with('っ') {
            return Err(Refusal::Whole);
        }
        let head = &text[start..bounds[a].end];
        let before = &text[bounds[a].start..start];
        let after = &text[end..bounds[b].end];
        // に and さん that the converter left in kana at the end are a
        // particle and a title: 八三三に, 一さんという.
        let word = &text[parse.words[parse.words.len() - 2].0..end];
        if matches!(word, "に" | "さん") && parts[b].value.ends_with(&format!("{word}{after}")) {
            return Err(Refusal::Shorter);
        }
        let mut borrowed = 0;
        let mut left = None;
        if !before.is_empty() {
            let shown = parts[a].value.as_str();
            let value = match shown.strip_suffix(head).filter(|kept| !kept.is_empty()) {
                // The converter attached the first numeral words as a particle.
                Some(kept) => {
                    borrowed = head.len();
                    kept.to_owned()
                }
                None => self.beside_numeral(parse, a, false).ok_or(Refusal::Whole)?,
            };
            left = Some(Segment {
                reading: before.to_owned(),
                value,
                ..Default::default()
            });
        }
        let mut right = None;
        if !after.is_empty() {
            let shown = parts[b].value.as_str();
            let value = if shown.len() > after.len() && shown.ends_with(after) {
                after.to_owned()
            } else {
                self.beside_numeral(parse, b, true).ok_or(Refusal::Whole)?
            };
            right = Some(Segment {
                reading: after.to_owned(),
                value,
                ..Default::default()
            });
        }
        // What is displayed where the number is read.
        let covered: String = parts[a..=b].iter().map(|s| s.value.as_str()).collect();
        let from = left.as_ref().map_or(0, |s| s.value.len());
        let to = covered.len() - right.as_ref().map_or(0, |s| s.value.len());
        let shown = &covered[from..to];
        // The converter already wrote a number here, after a particle at most.
        // How it is written is the numeral comparison's concern.
        if written_as_number(shown) || written_as_number(&shown[borrowed..]) {
            return Err(Refusal::Whole);
        }
        // 二重 and 三重 are words of their own.
        let mut chars = shown.chars();
        if chars
            .next()
            .is_some_and(|c| "一二三四五六七八九".contains(c))
            && chars.next() == Some('重')
            && chars.next().is_none()
        {
            return Err(Refusal::Whole);
        }
        // A row of digit words is common in ordinary text: 午後, いちご. It is a
        // number only where the converter itself wrote one of them as a numeral.
        let digit = |index: usize| {
            let (reading, value) = (parts[index].reading.as_str(), parts[index].value.as_str());
            whole(index)
                && DIGITS
                    .iter()
                    .chain([&ZERO])
                    .any(|(word, _)| *word == reading)
                && (written_as_number(value) || matches!(value, "ゼロ" | "零"))
        };
        if !parse.positional && !(a..=b).any(digit) {
            return Err(Refusal::Whole);
        }
        let kanji = parse.kanji(0..parse.words.len());
        let digits = numerals::value(&kanji).ok_or(Refusal::Whole)?;
        let number = Number {
            range: a..b + 1,
            reading: text[start..end].to_owned(),
            spellings: [kanji, digits],
            left,
            right,
        };
        // A borrowed particle may be one after all: also read the number without it.
        let resume = if borrowed > 0 { start + borrowed } else { end };
        Ok((number, resume))
    }

    // How the kana beside the number in segment `index` are written. A
    // candidate of the segment that spells the number's part with a numeral
    // shows them, when the displayed spelling writes them the same way: 級号の
    // has 九号の, so ごうの is 号の. With `leading` the number starts the segment.
    fn beside_numeral(&self, parse: &Parse, index: usize, leading: bool) -> Option<String> {
        // Only a number with units is told apart from an ordinary word this way.
        if !parse.positional {
            return None;
        }
        let bound = &self.bounds[index];
        let inside = |word: &(usize, char)| bound.start < word.0 && word.0 <= bound.end;
        let from = parse.words.iter().position(inside)?;
        let to = parse.words.iter().rposition(inside)?;
        // The segment begins or ends between two numeral words.
        let aligned = if leading {
            from.checked_sub(1).map(|i| parse.words[i].0) == Some(bound.start)
        } else {
            parse.words[to].0 == bound.end
        };
        if !aligned {
            return None;
        }
        let shown = self.parts[index].value.as_str();
        let spellings = parse.spellings(from..to + 1);
        let listed = self.lists.get(index).into_iter().flatten();
        let values = std::iter::once(shown).chain(
            listed
                .filter(|c| !c.transliteration)
                .map(|c| c.value.as_str()),
        );
        for value in values {
            for numeral in &spellings {
                let kept = if leading {
                    value.strip_prefix(numeral.as_str())
                } else {
                    value.strip_suffix(numeral.as_str())
                };
                let Some(kept) = kept.filter(|kept| !kept.is_empty()) else {
                    continue;
                };
                let same = if leading {
                    shown.ends_with(kept)
                } else {
                    shown.starts_with(kept)
                };
                if same && shown.len() > kept.len() {
                    return Some(kept.to_owned());
                }
            }
        }
        None
    }
}

/// Numbers of two or more numeral words read across the segments of a
/// clause, each with the cut that makes it one segment. The whole clause is
/// read at once: a part of its segments could show a part of a number.
pub(super) fn numbers(parts: &[Segment], lists: &[Vec<FocusCandidate>]) -> Vec<Number> {
    let mut bounds = Vec::with_capacity(parts.len());
    let mut text = String::new();
    for part in parts {
        bounds.push(text.len()..text.len() + part.reading.len());
        text.push_str(&part.reading);
    }
    let reading = Reading {
        text,
        bounds,
        parts,
        lists,
    };
    let text = &reading.text;
    let mut found = Vec::new();
    // A refused number reaches this far. A shorter number inside it would be
    // a fragment: 964 of 1964.
    let mut refused = 0;
    let mut at = 0;
    while at < text.len() {
        let parses = parses(text, at);
        let mut accepted = None;
        for parse in &parses {
            if parse.end() <= refused {
                break;
            }
            match reading.fit(at, parse) {
                Ok(fitted) => {
                    accepted = Some(fitted);
                    break;
                }
                Err(Refusal::Shorter) => {}
                Err(Refusal::Whole) => break,
            }
        }
        if let Some((number, resume)) = accepted {
            found.push(number);
            at = resume;
        } else {
            if let Some(longest) = parses.first() {
                refused = refused.max(longest.end());
            }
            at += text[at..].chars().next().map_or(1, char::len_utf8);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments(words: &[(&str, &str)]) -> Vec<Segment> {
        words
            .iter()
            .map(|(reading, value)| Segment {
                reading: (*reading).into(),
                value: (*value).into(),
                ..Default::default()
            })
            .collect()
    }

    // The numbers of a conversion: the segments each replaces and their text
    // with kanji numerals and with digits. `listed` adds candidates to a segment.
    fn offered_with(
        words: &[(&str, &str)],
        listed: &[(usize, &[&str])],
    ) -> Vec<(Range<usize>, String, String)> {
        let parts = segments(words);
        let mut lists: Vec<_> = parts.iter().map(|p| vec![choice(&p.value)]).collect();
        for (index, values) in listed {
            lists[*index].extend(values.iter().map(|value| choice(value)));
        }
        numbers(&parts, &lists)
            .iter()
            .map(|number| {
                let [kanji, digits] = number.spellings();
                (
                    number.range.clone(),
                    number.written(kanji),
                    number.written(digits),
                )
            })
            .collect()
    }

    fn offered(words: &[(&str, &str)]) -> Vec<(Range<usize>, String, String)> {
        offered_with(words, &[])
    }

    fn one(range: Range<usize>, kanji: &str, digits: &str) -> Vec<(Range<usize>, String, String)> {
        vec![(range, kanji.to_owned(), digits.to_owned())]
    }

    #[test]
    fn spoken_numbers_have_a_value() {
        let read = |text: &str| {
            let parse = parses(text, 0).into_iter().next()?;
            let kanji = parse.kanji(0..parse.words.len());
            (parse.end() == text.len()).then(|| (numerals::value(&kanji).unwrap(), kanji))
        };
        for (text, digits, kanji) in [
            ("せんきゅうひゃくろくじゅうきゅう", "1969", "千九百六十九"),
            ("にせんにじゅうに", "2022", "二千二十二"),
            ("さんぜんろっぴゃくはちじゅう", "3680", "三千六百八十"),
            ("はっせんはっぴゃく", "8800", "八千八百"),
            ("せんろく", "1006", "千六"),
            ("ひゃくろく", "106", "百六"),
            ("じゅういち", "11", "十一"),
            ("じゅうしち", "17", "十七"),
            ("にじゅう", "20", "二十"),
            ("はちいち", "81", "八一"),
            ("にぜろぜろ", "200", "二〇〇"),
            ("いちろくろく", "166", "一六六"),
        ] {
            assert_eq!(
                read(text),
                Some((digits.to_owned(), kanji.to_owned())),
                "{text}"
            );
        }
        // One numeral word is not a number here, nor is a row that starts with zero.
        for text in ["せん", "じゅう", "いち", "ぜろぜろ", "さんせん", "にし"] {
            assert_eq!(read(text), None, "{text}");
        }
        // The shorter readings follow the longest one.
        let ends: Vec<_> = parses("にじゅうに", 0).iter().map(Parse::end).collect();
        assert_eq!(ends, ["にじゅうに".len(), "にじゅう".len()]);
        // Four is よ only before 年.
        assert_eq!(parses("にじゅうよねん", 0)[0].kanji(0..3), "二十四");
        assert_eq!(parses("にじゅうよじ", 0)[0].kanji(0..2), "二十");
    }

    #[test]
    fn a_number_read_as_separate_words_is_cut_as_one_segment() {
        assert_eq!(
            offered(&[
                ("せんきゅう", "船級"),
                ("ひゃく", "百"),
                ("ろく", "六"),
                ("じゅう", "重"),
                ("きゅう", "九"),
                ("ねんより", "年より"),
            ]),
            one(0..5, "千九百六十九", "1969")
        );
        assert_eq!(
            offered(&[
                ("さん", "三"),
                ("じゅう", "重"),
                ("さん", "三"),
                ("ねん", "年")
            ]),
            one(0..3, "三十三", "33")
        );
        assert_eq!(
            offered(&[
                ("かいは", "会は"),
                ("じゅう", "中"),
                ("ご", "五"),
                ("にち", "日")
            ]),
            one(1..3, "十五", "15")
        );
        // A row of digits, where the converter wrote one of them as a numeral.
        assert_eq!(
            offered(&[("はち", "八"), ("いち", "市"), ("せたい", "世帯")]),
            one(0..2, "八一", "81")
        );
        assert_eq!(
            offered(&[
                ("だい", "第"),
                ("いち", "一"),
                ("ろくろく", "碌々"),
                ("しが", "氏が")
            ]),
            one(1..3, "一六六", "166")
        );
        // The cut has the number as one segment with one spelling.
        let parts = segments(&[("はち", "八"), ("いち", "市"), ("せたい", "世帯")]);
        let lists: Vec<_> = parts.iter().map(|p| vec![choice(&p.value)]).collect();
        let found = numbers(&parts, &lists);
        let (cut, choices) = found[0].cut("81");
        assert_eq!(cut.len(), 1);
        assert_eq!(
            (cut[0].reading.as_str(), cut[0].value.as_str()),
            ("はちいち", "81")
        );
        assert_eq!(choices[0].len(), 1);
    }

    #[test]
    fn the_kana_around_a_number_keep_their_spelling() {
        // A first digit that the converter attached to the word before it.
        assert_eq!(
            offered(&[
                ("りーぐせんに", "リーグ戦に"),
                ("ぜろ", "ゼロ"),
                ("ぜろ", "零"),
                ("しあい", "試合"),
            ]),
            one(0..3, "リーグ戦二〇〇", "リーグ戦200")
        );
        // Kana after the last digit.
        assert_eq!(
            offered(&[("よん", "四"), ("ぜろにて", "ゼロにて")]),
            one(0..2, "四〇にて", "40にて")
        );
        assert_eq!(
            offered(&[("いち", "一"), ("ぜろ", "ゼロ"), ("ぜろの", "ゼロの")]),
            one(0..3, "一〇〇の", "100の")
        );
        // A counter that the converter wrote after the last numeral.
        assert_eq!(
            offered(&[
                ("さん", "三"),
                ("じゅう", "重"),
                ("ろくにんしゅうの", "六人衆の")
            ]),
            one(0..3, "三十六人衆の", "36人衆の")
        );
        assert_eq!(
            offered(&[("にじゅう", "二重"), ("よねん", "四年")]),
            one(0..2, "二十四年", "24年")
        );
        // Another candidate of the segment shows where its numeral ends.
        assert_eq!(
            offered_with(
                &[
                    ("さんびゃく", "三百"),
                    ("ごじゅう", "五十"),
                    ("きゅうごうの", "級号の")
                ],
                &[(2, &["旧号の", "九号の"])],
            ),
            one(0..3, "三百五十九号の", "359号の")
        );
        assert_eq!(
            offered_with(&[("に", "二"), ("じゅうにん", "住人")], &[(1, &["十人"])],),
            one(0..2, "二十人", "20人")
        );
        assert_eq!(
            offered_with(
                &[("だいじゅう", "第中"), ("に", "二"), ("じょう", "条")],
                &[(0, &["大銃", "第十"])],
            ),
            one(0..2, "第十二", "第12")
        );
        // The remaining kana become a segment of their own.
        let parts = segments(&[("よん", "四"), ("ぜろにて", "ゼロにて")]);
        let lists: Vec<_> = parts.iter().map(|p| vec![choice(&p.value)]).collect();
        let (cut, _) = numbers(&parts, &lists)[0].cut("40");
        let cut: Vec<_> = cut
            .iter()
            .map(|s| (s.reading.as_str(), s.value.as_str()))
            .collect();
        assert_eq!(cut, [("よんぜろ", "40"), ("にて", "にて")]);
    }

    #[test]
    fn a_particle_before_a_number_is_read_both_ways() {
        // の and 2006, or のに and 1006: the model compares them.
        assert_eq!(
            offered(&[
                ("のに", "のに"),
                ("せん", "線"),
                ("ろく", "六"),
                ("ねんど", "年度")
            ]),
            [
                (0..3, "の二千六".to_owned(), "の2006".to_owned()),
                (1..3, "千六".to_owned(), "1006".to_owned()),
            ]
        );
        // After the particle the converter already wrote a number.
        for words in [
            &[("すでに", "すでに"), ("ご", "五"), ("ねんほど", "年ほど")][..],
            &[
                ("れいじょうに", "霊場に"),
                ("じゅう", "十"),
                ("さん", "三"),
                ("ばん", "番"),
            ],
            &[
                ("たなかさん", "田中さん"),
                ("じゅう", "十"),
                ("ご", "五"),
                ("さい", "歳"),
            ],
        ] {
            assert!(offered(words).is_empty(), "{words:?}");
        }
    }

    #[test]
    fn words_that_read_like_numbers_are_left_alone() {
        for words in [
            // One segment is the converter's word: 二重に is not 22.
            &[("それを", "それを"), ("にじゅうにする", "二重にする")][..],
            &[("にじゅうに", "二重に"), ("かける", "掛ける")],
            &[("さんじゅうしの", "三銃士の"), ("ものがたり", "物語")],
            &[("せかいじゅうに", "世界中に"), ("ひろがる", "広がる")],
            &[
                ("ごじゅうおんじゅんに", "五十音順に"),
                ("ならべる", "並べる"),
            ],
            // 三重 before a word, although its kana cross two segments.
            &[
                ("かなだ", "カナダ"),
                ("さん", "三"),
                ("じゅう", "重"),
                ("そうだんを", "奏団を"),
            ],
            // The converter already wrote the number.
            &[
                ("ろく", "六"),
                ("じゅう", "十"),
                ("に", "二"),
                ("にん", "人"),
            ],
            &[("いち", "一"), ("ろく", "六"), ("しーずん", "シーズン")],
            // The kana of two words: no segment is all number.
            &[("ひんぱんに", "頻繁に"), ("せんしゅを", "選手を")],
            &[("とうろく", "登録"), ("せんしゅとして", "選手として")],
            // Digit words without a numeral, and beside a counter of their own.
            &[("ご", "語"), ("ろく", "録"), ("を", "を")],
            &[("いち", "一"), ("にち", "日"), ("へいきん", "平均")],
            // に and さん in kana after a numeral are a particle and a title.
            &[
                ("いち", "一"),
                ("さんという", "さんという"),
                ("むらの", "村の"),
            ],
            &[("はち", "八"), ("さん", "さん"), ("さんに", "さんに")],
            // じゅうに and ん are not twelve.
            &[("じゅう", "中"), ("にん", "人")],
        ] {
            assert!(offered(words).is_empty(), "{words:?}");
        }
        // A candidate with a numeral counts only where the display agrees on the rest.
        assert!(
            offered_with(
                &[("に", "二"), ("せんしゅを", "選手を")],
                &[(1, &["千秋を"])],
            )
            .is_empty()
        );
        assert!(
            offered_with(
                &[("だい", "第"), ("に", "二"), ("きゅうじょうで", "球場で")],
                &[(2, &["九条で"])],
            )
            .is_empty()
        );
    }

    #[test]
    fn a_part_of_a_longer_number_is_not_a_number() {
        // 1964 cannot be cut here. Neither 1900 nor 964 is offered instead.
        assert!(
            offered(&[
                ("せんきゅう", "船級"),
                ("ひゃく", "百"),
                ("ろく", "六"),
                ("じゅうよねんに", "重余年に"),
            ])
            .is_empty()
        );
        // The last digits alone would be a number: the whole clause is read.
        let tail = [
            ("ろく", "六"),
            ("じゅう", "重"),
            ("きゅう", "九"),
            ("ねん", "年"),
        ];
        assert_eq!(offered(&tail), one(0..3, "六十九", "69"));
    }
}
