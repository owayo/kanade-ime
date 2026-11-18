// SPDX-License-Identifier: MIT
//! Prompt formats. Every registered profile owns one [`Prompt`]; no marker,
//! space rule or reading script is shared between models. Changing a format
//! changes its digest, which invalidates the qualifications bound to it.
use crate::{Job, limits};
use sha2::{Digest, Sha256};

/// Rules the worker applies to every profile. They are part of every prompt
/// digest, so changing one invalidates every stored qualification.
pub const RULES: &str = "\
layout=[start token],[context-marker context],input-marker reading,output-marker surface,END\n\
linebreaks=CR and LF are dropped\n\
tokenize=add_special:false,parse_special:false,conditioning and continuation separately\n\
score=sum of natural-log probabilities of the differing continuation and END\n\
unrepresentable=context cut after the last such char; candidate unscored\n\
reserved=a request containing a marker or a reserved character is refused, never altered\n";

/// What becomes of an ASCII space in context and surface text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    /// Replaced by U+3000, as in training text without ASCII spaces.
    Ideographic,
    Keep,
}
/// Script in which the model expects the reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Script {
    Katakana,
    Hiragana,
}

/// One model family's conditioning format.
#[derive(Debug, PartialEq, Eq)]
pub struct Prompt {
    pub version: u32,
    /// Precedes the left context; omitted together with an empty context.
    pub context: &'static str,
    /// Precedes the reading.
    pub input: &'static str,
    /// Precedes the surface the model continues with.
    pub output: &'static str,
    pub space: Space,
    pub reading: Script,
    /// Inclusive character ranges that request text may never contain, in
    /// addition to the three markers themselves.
    pub reserved: &'static [(char, char)],
}

impl Prompt {
    pub fn markers(&self) -> [&'static str; 3] {
        [self.context, self.input, self.output]
    }
    /// Context and surface text as the model sees it.
    pub fn normalize(&self, text: &str) -> String {
        text.chars()
            .filter(|c| !matches!(c, '\n' | '\r'))
            .map(|c| match self.space {
                Space::Ideographic if c == ' ' => '\u{3000}',
                _ => c,
            })
            .collect()
    }
    /// A job's katakana reading in the script of this format.
    pub fn reading(&self, katakana: &str) -> String {
        match self.reading {
            Script::Katakana => katakana.to_owned(),
            Script::Hiragana => katakana
                .chars()
                .map(|c| match c {
                    'ァ'..='ヶ' => char::from_u32(c as u32 - 0x60).unwrap_or(c),
                    _ => c,
                })
                .collect(),
        }
    }
    /// Conditioning text from an already normalized context and a converted
    /// reading. The model then continues with the surface and the end token.
    pub fn conditioning(&self, context: &str, reading: &str) -> String {
        let mut prompt = String::with_capacity(
            context.len() + reading.len() + self.markers().iter().map(|m| m.len()).sum::<usize>(),
        );
        if !context.is_empty() {
            prompt.push_str(self.context);
            prompt.push_str(context);
        }
        prompt.push_str(self.input);
        prompt.push_str(reading);
        prompt.push_str(self.output);
        prompt
    }
    /// Whether `text` contains a marker or a reserved character.
    pub fn reserves(&self, text: &str) -> bool {
        text.chars().any(|c| {
            self.reserved
                .iter()
                .any(|(first, last)| (*first..=*last).contains(&c))
        }) || self.markers().iter().any(|marker| text.contains(marker))
    }
    /// Whether every text of the job is free of markers and reserved
    /// characters. A refused job is never scored and never altered.
    ///
    /// The surface is examined as the model sees it, prefix and suffix joined
    /// to each candidate, so a marker cannot be assembled across two fields.
    pub fn admits(&self, job: &Job) -> bool {
        let fields = [&job.context, &job.reading, &job.prefix, &job.suffix];
        if fields
            .into_iter()
            .chain(&job.candidates)
            .any(|text| self.reserves(text))
        {
            return false;
        }
        // One-character markers cannot span a field boundary.
        if self.markers().iter().all(|m| m.chars().count() == 1) {
            return true;
        }
        !self.reserves(&self.reading(&job.reading))
            && job.candidates.iter().all(|candidate| {
                let surface = [job.prefix.as_str(), candidate, &job.suffix].concat();
                !self.reserves(&self.normalize(&surface))
            })
    }
    /// Structural requirements that make `admits` sufficient: text next to a
    /// marker cannot complete another marker, and normalization cannot create
    /// one.
    pub fn well_formed(&self) -> bool {
        let markers = self.markers();
        let plain = |marker: &str| {
            !marker.is_empty()
                && marker.len() <= 16
                && !marker
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace() || matches!(c, 'ぁ'..='ヺ' | 'ー'))
        };
        // No proper suffix of one marker starts another (or the same) marker.
        let overlaps = |a: &str, b: &str| {
            a.char_indices()
                .skip(1)
                .any(|(index, _)| b.starts_with(&a[index..]))
        };
        self.version > 0
            && markers.iter().all(|marker| plain(marker))
            && markers.iter().enumerate().all(|(i, a)| {
                markers
                    .iter()
                    .enumerate()
                    .all(|(j, b)| !overlaps(a, b) && (i == j || !a.contains(b)))
            })
            && self.reserved.iter().all(|(first, last)| {
                first <= last
                    && !(*first..=*last).contains(&' ')
                    && !(*first..=*last).contains(&'\u{3000}')
            })
    }
    /// Digest of every rule that affects a score under this format.
    pub fn sha256(&self) -> String {
        let code = |text: &str| {
            text.chars()
                .map(|c| format!("U+{:04X}", c as u32))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let mut spec = format!(
            "kanade-llm-prompt v{}\n{RULES}markers=context:{};input:{};output:{}\nspace={:?}\nreading={:?}\ncontext=last {} chars\nreserved=",
            self.version,
            code(self.context),
            code(self.input),
            code(self.output),
            self.space,
            self.reading,
            limits::CONTEXT_CHARS,
        );
        for (first, last) in self.reserved {
            spec.push_str(&format!("U+{:04X}-U+{:04X};", *first as u32, *last as u32));
        }
        spec.push('\n');
        crate::profile::hex(&Sha256::digest(spec.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{self, fixture};

    fn tsumugi() -> &'static Prompt {
        &profile::CATALOG[0].profile.prompt
    }
    fn job() -> Job {
        Job {
            context: "今日は".into(),
            reading: "コウエン".into(),
            prefix: "市の".into(),
            suffix: "で".into(),
            candidates: vec!["公園".into(), "講演".into()],
        }
    }
    #[test]
    fn the_existing_model_keeps_its_reference_layout() {
        let prompt = tsumugi();
        assert_eq!(
            prompt.conditioning("今日は", &prompt.reading("コウエン")),
            "\u{E080}今日は\u{E081}コウエン\u{E082}"
        );
        assert_eq!(
            prompt.conditioning("", "コウエン"),
            "\u{E081}コウエン\u{E082}"
        );
        assert_eq!(prompt.normalize("a b\r\nc"), "a\u{3000}bc");
        assert_eq!(prompt.normalize("今日は\u{3000}晴れ"), "今日は\u{3000}晴れ");
        assert!(prompt.well_formed());
    }
    #[test]
    fn each_profile_formats_with_its_own_markers_space_rule_and_script() {
        let alt = &fixture::ALT_PROFILE.prompt;
        assert_eq!(
            alt.conditioning("今日は", &alt.reading("コウエンヴー")),
            "『今日は「こうえんゔー』"
        );
        assert_eq!(alt.normalize("a b\nc"), "a bc");
        let tags = &fixture::OTHER_PROFILE.prompt;
        assert_eq!(
            tags.conditioning("今日は", &tags.reading("コウエン")),
            "<ctx>今日は<in>コウエン<out>"
        );
        let digests = [tsumugi().sha256(), alt.sha256(), tags.sha256()];
        assert!(digests.iter().all(|d| profile::is_sha256(d)));
        assert!(digests[0] != digests[1] && digests[1] != digests[2] && digests[0] != digests[2]);
        assert!(alt.well_formed() && tags.well_formed());
    }
    #[test]
    fn every_format_rule_is_part_of_the_digest() {
        let base = || Prompt {
            version: 1,
            context: "\u{E080}",
            input: "\u{E081}",
            output: "\u{E082}",
            space: Space::Ideographic,
            reading: Script::Katakana,
            reserved: &[('\u{E080}', '\u{E08F}')],
        };
        assert_eq!(base().sha256(), tsumugi().sha256());
        let edits: [fn(&mut Prompt); 7] = [
            |p| p.version = 2,
            |p| p.context = "\u{E083}",
            |p| p.input = "\u{E083}",
            |p| p.output = "\u{E083}",
            |p| p.space = Space::Keep,
            |p| p.reading = Script::Hiragana,
            |p| p.reserved = &[('\u{E080}', '\u{E082}')],
        ];
        for edit in edits {
            let mut changed = base();
            edit(&mut changed);
            assert_ne!(changed.sha256(), base().sha256());
        }
    }
    #[test]
    fn a_marker_in_any_field_refuses_the_job_without_altering_it() {
        let prompt = tsumugi();
        assert!(prompt.admits(&job()));
        type Edit = fn(&mut Job, &str);
        let fields: [Edit; 5] = [
            |j, m| j.context.push_str(m),
            |j, m| j.reading.push_str(m),
            |j, m| j.prefix.push_str(m),
            |j, m| j.suffix.insert_str(0, m),
            |j, m| j.candidates[1].push_str(m),
        ];
        // The three markers and the rest of the reserved block.
        for marker in ["\u{E081}", "\u{E082}", "\u{E080}", "\u{E083}", "\u{E08F}"] {
            for edit in fields {
                let mut bad = job();
                edit(&mut bad, marker);
                let before = bad.clone();
                assert!(!prompt.admits(&bad));
                assert_eq!(bad, before);
            }
        }
        // Outside the block, and another profile's markers, are ordinary text.
        for ordinary in ["\u{E090}", "\u{E07F}", "「", "<in>", "</s>"] {
            let mut fine = job();
            fine.candidates[0].push_str(ordinary);
            assert!(prompt.admits(&fine));
        }
        // The same text is refused by the profile that reserves it, and that
        // profile accepts what the first one reserves.
        let alt = &fixture::ALT_PROFILE.prompt;
        let mut bracket = job();
        bracket.candidates[0].push('「');
        assert!(prompt.admits(&bracket) && !alt.admits(&bracket));
        let mut private = job();
        private.context.push('\u{E080}');
        assert!(!prompt.admits(&private) && alt.admits(&private));
    }
    #[test]
    fn a_marker_assembled_across_fields_is_refused() {
        let tags = &fixture::OTHER_PROFILE.prompt;
        assert!(tags.admits(&job()));
        let mut split = job();
        split.prefix = "<i".into();
        split.candidates = vec!["n>".into(), "講演".into()];
        assert!(!tags.reserves("<i") && !tags.reserves("n>"));
        assert!(!tags.admits(&split));
        let mut tail = job();
        tail.candidates[1] = "講演<ou".into();
        tail.suffix = "t>".into();
        assert!(!tags.admits(&tail));
    }
    #[test]
    fn ambiguous_formats_are_not_well_formed() {
        let prompt = |context, input, output| Prompt {
            version: 1,
            context,
            input,
            output,
            space: Space::Keep,
            reading: Script::Katakana,
            reserved: &[],
        };
        assert!(prompt("<c>", "<i>", "<o>").well_formed());
        for (context, input, output) in [
            ("", "<i>", "<o>"),
            ("<i>", "<i>", "<o>"),
            // A marker inside another, a self-overlap and a mutual overlap.
            ("<i>x", "<i>", "<o>"),
            ("abab", "<i>", "<o>"),
            ("<c>|", "|<i>", "<o>"),
            // Text that normalization or a reading could produce.
            ("a b", "<i>", "<o>"),
            ("a\u{3000}b", "<i>", "<o>"),
            ("a\nb", "<i>", "<o>"),
            ("ア", "<i>", "<o>"),
        ] {
            assert!(!prompt(context, input, output).well_formed());
        }
        let mut reserved = prompt("<c>", "<i>", "<o>");
        reserved.reserved = &[(' ', '~')];
        assert!(!reserved.well_formed());
        reserved.reserved = &[('b', 'a')];
        assert!(!reserved.well_formed());
        reserved.version = 0;
        assert!(!reserved.well_formed());
    }
}
