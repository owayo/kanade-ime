// SPDX-License-Identifier: MIT
//! Score an initial conversion against alternatives from a disposable Mozc session.
mod boundary_data;
mod calendar;
mod clause;
mod cut_data;
mod edit_data;
mod numerals;
pub(super) mod probe;
mod search;
mod spelling_data;
mod spoken;

use super::*;
use kanade_llm::{Job, Poll, Rerank, limits, profile::Preview};
use kanade_mozc::FocusCandidate;
use std::time::Duration;

/// Windows scored for one conversion: every clause, and each part of a long clause.
const MAX_WINDOWS: usize = 4;
/// A later window is submitted only this early in the preview's lifetime.
const SCAN_START_MS: u64 = 350;
/// Changes already chosen are shown instead of waiting longer for another window.
const SCAN_APPLY_MS: u64 = 600;
/// Thresholds that the scripted scores of the unit tests are written against.
/// Registering other weights changes none of their expectations.
#[cfg(test)]
pub(crate) const TEST_POLICY: Preview = Preview {
    sentence: 5.0,
    clause: 7.0,
    runner_up: 0.75,
    added: 2.0,
    ending: 1.5,
    rank: 0.75,
    digits_seen: 0.0,
    digits: 3.0,
    shortlist: 6,
    shortlist_cost: 32,
    rounds: 1,
    katakana: 8,
};

/// A segment whose displayed spelling a candidate replaces.
type Change = (usize, String);

/// The segments that one window scores, and their dictionary candidates.
#[derive(Clone)]
struct Lattice {
    parts: Vec<Segment>,
    lists: Vec<Vec<FocusCandidate>>,
    /// Numbers read across these segments, found in the whole clause.
    numbers: Vec<spoken::Number>,
}

/// A number read across segments, and the candidates that write it with
/// kanji numerals and with ASCII digits.
struct Spoken {
    number: spoken::Number,
    kanji: Option<usize>,
    digits: Option<usize>,
    /// The displayed text with the numerals of the number's segments in
    /// digits, when it shows any: 4級から for 四級から.
    shown_digits: Option<String>,
}

struct Proposal {
    job: Job,
    before: String,
    after: String,
    penalties: Vec<f32>,
    margin: f32,
    original_count: usize,
    /// Compares ways to write the numbers of a spelling that is already chosen.
    numerals: bool,
    /// For each candidate, the segments it respells. `None` when it moves a
    /// segment boundary or was not built from the window's segments.
    changes: Vec<Option<Vec<Change>>>,
    /// Numbers read across segments, each offered in two notations.
    numbers: Vec<Spoken>,
    /// Candidates that are scored only to compare a number's digits with:
    /// the display with its own numerals in digits. They are never chosen.
    references: Vec<usize>,
    /// Kanji numerals of such numbers accepted in an earlier look at this
    /// window. Whether to write them with digits is still to be compared.
    undecided: Vec<String>,
    /// The scored segments and their dictionary candidates.
    lattice: Option<Lattice>,
    /// How many spellings of this window have been accepted before.
    round: u8,
    /// Thresholds of the model that scores the candidates.
    policy: Preview,
}

impl Proposal {
    fn render(&self, index: usize) -> String {
        format!(
            "{}{}{}{}",
            self.before, self.job.prefix, self.job.candidates[index], self.after
        )
    }

    // The number that a candidate writes, and how it writes it.
    fn spoken(&self, index: usize) -> Option<(&spoken::Number, &str)> {
        self.numbers.iter().find_map(|spoken| {
            let [kanji, digits] = spoken.number.spellings();
            if spoken.kanji == Some(index) {
                Some((&spoken.number, kanji.as_str()))
            } else if spoken.digits == Some(index) {
                Some((&spoken.number, digits.as_str()))
            } else {
                None
            }
        })
    }

    // The lead that digits need over kanji numerals. Committed text that shows
    // ASCII digits has settled the style.
    fn digit_lead(&self, committed: &str) -> f32 {
        if committed.bytes().any(|b| b.is_ascii_digit()) {
            self.policy.digits_seen
        } else {
            self.policy.digits
        }
    }

    // The kanji numerals of numbers read from the kana whose notation is
    // open once candidate `winner` is accepted.
    fn open_notation(&self, winner: usize) -> Vec<String> {
        let mut spoken = self.undecided.clone();
        if let Some((number, spelling)) = self.spoken(winner)
            && *spelling == number.spellings()[0]
        {
            spoken.push(spelling.to_owned());
        }
        spoken
    }

    // A text with one confirmed correction often needs another. Look once
    // more around the accepted spelling, with the same context and reading.
    fn next_round(&self, winner: usize) -> Option<Proposal> {
        if self.numerals || self.round >= self.policy.rounds || winner == 0 {
            return None;
        }
        let mut lattice = self.lattice.clone()?;
        if let Some((number, spelling)) = self.spoken(winner) {
            // The accepted number is one segment from here on. Another reading
            // of its segments is settled with it; later numbers move along.
            let range = number.range.clone();
            let (parts, lists) = number.cut(spelling);
            let cut = parts.len();
            lattice.parts.splice(range.clone(), parts);
            lattice.lists.splice(range.clone(), lists);
            lattice
                .numbers
                .retain(|other| other.range.end <= range.start || range.end <= other.range.start);
            for other in &mut lattice.numbers {
                if other.range.start >= range.end {
                    let moved = |at: usize| at - range.len() + cut;
                    other.range = moved(other.range.start)..moved(other.range.end);
                }
            }
        } else {
            let changes = self.changes.get(winner)?.as_ref()?;
            // A number is read from the spellings that were displayed.
            lattice
                .numbers
                .retain(|number| !changes.iter().any(|(pos, _)| number.range.contains(pos)));
            for (pos, value) in changes {
                lattice.parts[*pos].value.clone_from(value);
                let list = &mut lattice.lists[*pos];
                let chosen = match list.iter().position(|c| c.value == *value) {
                    Some(index) => list.remove(index),
                    None => FocusCandidate {
                        reading: String::new(),
                        annotation: String::new(),
                        value: value.clone(),
                        transliteration: false,
                        history_deletable: false,
                    },
                };
                list.insert(0, chosen);
            }
        }
        let job = Job {
            candidates: Vec::new(),
            ..self.job.clone()
        };
        let Lattice {
            parts,
            lists,
            numbers,
        } = &lattice;
        let mut item = contextual_proposal(
            parts,
            lists,
            job,
            &self.policy,
            numbers,
            &self.before,
            &self.after,
        );
        search::paired(&mut item, parts, lists);
        item.before.clone_from(&self.before);
        item.after.clone_from(&self.after);
        item.margin = self.margin;
        item.round = self.round + 1;
        item.undecided = self.open_notation(winner);
        item.lattice = Some(lattice);
        item.job.valid_for(true).then_some(item)
    }

    // After the words are chosen, the model compares the same text with its
    // numbers in digits. Both have one reading, so only the context decides.
    fn numeral_choice(&self, winner: usize, committed: &str) -> Option<Proposal> {
        if self.numerals || self.job.context.is_empty() || !self.job.prefix.is_empty() {
            return None;
        }
        let text = &self.job.candidates[winner];
        let mut candidates = vec![text.clone()];
        for variant in numerals::variants(text, &self.after, &self.open_notation(winner)) {
            if variant.chars().count() <= limits::CANDIDATE_CHARS && !candidates.contains(&variant)
            {
                candidates.push(variant);
            }
        }
        let count = candidates.len();
        let job = Job {
            candidates,
            ..self.job.clone()
        };
        (count > 1 && job.valid_for(true)).then(|| Proposal {
            job,
            before: self.before.clone(),
            after: self.after.clone(),
            penalties: vec![0.0; count],
            margin: self.digit_lead(committed),
            original_count: count,
            numerals: true,
            changes: vec![None; count],
            numbers: Vec::new(),
            references: Vec::new(),
            undecided: Vec::new(),
            lattice: None,
            round: self.round,
            policy: self.policy,
        })
    }

    // Whether candidate `index` may replace the display under the safeguards
    // for names. A number heeds only the amount of katakana around it.
    fn admitted(&self, index: usize) -> bool {
        if self.job.context.is_empty() {
            return true;
        }
        let (shown, chosen) = (self.render(0), self.render(index));
        if self.spoken(index).is_some() {
            number_is_trusted(&shown, &chosen, self.policy.katakana)
        } else {
            preserves_name_spelling(&shown, &chosen, self.policy.katakana)
        }
    }

    // Only change the provisional spelling with a clear advantage over
    // both Mozc's default and the other spellings under consideration.
    // `committed` is the text before the conversion.
    fn winner(&self, scores: &[Option<f32>], committed: &str) -> Option<usize> {
        let job = &self.job;
        if scores.len() != job.candidates.len()
            || scores[..self.original_count].iter().any(Option::is_none)
            || scores.iter().flatten().any(|s| !s.is_finite())
        {
            return None;
        }
        if self.numerals {
            // Every candidate is scored: the check above covers the whole list.
            let values: Vec<f32> = scores.iter().copied().collect::<Option<_>>()?;
            let best = (1..values.len()).max_by(|a, b| values[*a].total_cmp(&values[*b]))?;
            let lead = values[best] - values[0];
            return Some(if lead >= self.margin { best } else { 0 });
        }
        let raw = scores;
        let mut scores: Vec<_> = raw
            .iter()
            .zip(&self.penalties)
            .map(|(s, p)| s.map_or(f32::NEG_INFINITY, |v| v - p))
            .collect();
        // The two notations of a number are one alternative, not rivals. Its
        // digits stand for it only with their lead over the kanji numerals.
        let lead = self.digit_lead(committed);
        for spoken in &self.numbers {
            let Some(digits) = spoken.digits else {
                continue;
            };
            // Digits are compared with digits. Where the display shows a
            // numeral, the model favors any spelling with digits over it, so
            // 49 has to lead 4級 rather than 四級. Without that spelling of
            // the display, the kanji numerals are compared instead.
            let fair = spoken.shown_digits.as_ref().is_none_or(|shown| {
                job.candidates
                    .iter()
                    .position(|c| c == shown)
                    .and_then(|index| raw[index].zip(raw[digits]))
                    .is_some_and(|(shown, number)| number - shown >= self.margin)
            });
            // The notation that stands for the number has to be one that may
            // be displayed. Otherwise its kanji numerals would be dropped for
            // digits that the safeguards then refuse.
            let stands = fair
                && self.admitted(digits)
                && spoken
                    .kanji
                    .is_none_or(|kanji| scores[digits] - scores[kanji] >= lead);
            match spoken.kanji {
                Some(kanji) if stands => scores[kanji] = f32::NEG_INFINITY,
                _ if stands => {}
                _ => scores[digits] = f32::NEG_INFINITY,
            }
        }
        for &reference in &self.references {
            scores[reference] = f32::NEG_INFINITY;
        }
        let mut order: Vec<_> = (0..scores.len()).collect();
        order.sort_by(|a, b| scores[*b].total_cmp(&scores[*a]));
        let originals: Vec<_> = order
            .iter()
            .copied()
            .filter(|&i| i < self.original_count)
            .collect();
        let mut winner = 0;
        if let [first, second, ..] = originals[..] {
            let runner_up_margin = if job.context.is_empty() {
                self.margin
            } else {
                self.policy.runner_up * self.margin
            };
            if scores[first] - scores[0] >= self.margin
                && scores[first] - scores[second] >= runner_up_margin
            {
                winner = first;
            }
        }
        let best = order[0];
        let expanded_margin = self.margin + self.policy.added;
        if best >= self.original_count && scores[best] - scores[order[1]] >= expanded_margin {
            winner = best;
        }
        if winner != 0 && !self.admitted(winner) {
            winner = 0;
        }
        // While the words of the window are looked at again, the number is
        // written with kanji numerals. Its notation is compared once they are
        // settled, like that of every other number. So it is where the kanji
        // numerals were not scored beside the digits.
        if let Some(spoken) = self.numbers.iter().find(|s| s.digits == Some(winner))
            && let Some(kanji) = spoken.kanji
            && (self.round < self.policy.rounds || raw[kanji].is_none())
        {
            winner = kanji;
        }
        Some(winner)
    }
}

enum Scan {
    /// The search or a model job has not answered yet.
    Waiting,
    /// The text with every accepted change, if any window was scored.
    Finished(Option<String>, RefinementOutcome),
}

// Put the chosen spelling of each scored window back into the displayed text.
fn compose(base: &str, edits: &[(usize, usize, String)]) -> String {
    let mut ordered: Vec<_> = edits.iter().collect();
    ordered.sort_by_key(|edit| std::cmp::Reverse(edit.0));
    let mut text = base.to_owned();
    for (start, end, value) in ordered {
        text.replace_range(*start..*end, value);
    }
    text
}

// Keep the numerical value chosen by the converter. Use the width of digits
// already typed in committed text only when that text uses ASCII digits
// consistently. Digits in the conversion itself, such as a calendar year,
// are the same evidence: one text does not mix the two widths.
fn contextual_digits(surface: &str, committed: &str) -> Option<String> {
    let wide = |c: char| ('０'..='９').contains(&c);
    let ascii = |text: &str| text.bytes().any(|b| b.is_ascii_digit());
    if !surface.contains(wide) || !(ascii(committed) || ascii(surface)) || committed.contains(wide)
    {
        return None;
    }
    Some(
        surface
            .chars()
            .map(|c| {
                if wide(c) {
                    char::from(b'0' + (u32::from(c) - u32::from('０')) as u8)
                } else {
                    c
                }
            })
            .collect(),
    )
}

pub(super) struct Pending {
    started: Instant,
    context: String,
    year: Option<String>,
    search: Option<probe::Ticket>,
    work: Option<(u64, Proposal)>,
    /// Windows not yet scored, the next one last. `None` until the search answers.
    queue: Option<Vec<Proposal>>,
    /// The displayed text when the search answered.
    base: String,
    /// Byte ranges of `base` and the spelling chosen for each.
    edits: Vec<(usize, usize, String)>,
    evaluated: bool,
}

impl Pending {
    pub(super) fn prepare(session: &Session, preceding: &str, dates: bool) -> Option<Self> {
        let parts = &session.snapshot.preedit.segments;
        let whole = reading(&session.snapshot.preedit);
        let punctuation = whole.contains(['、', '。']);
        if session.manual
            || whole.chars().count() > limits::READING_CHARS
            || !kanade_llm::eligible(Privacy::Normal, &whole.replace(['、', '。'], ""), false)
        {
            return None;
        }
        let context = if preceding.is_empty() {
            String::new()
        } else {
            kanade_llm::context_window(preceding)?
        };
        let is_default = !session.automatic
            && session.choices.get(session.selected_choice)?.source == CandidateSource::Mozc;
        let year = if dates && is_default && whole.contains("ねん") {
            calendar::surface(parts, &context)
        } else {
            None
        };
        if year.is_none() {
            if context.is_empty() && Ending::parse(parts).is_some() {
                if punctuation && !is_default {
                    return None;
                }
            } else if !is_default
                || !(1..=24).contains(&parts.len())
                || (context.is_empty() && parts.len() == 1 && !punctuation)
            {
                return None;
            }
        }
        Some(Self {
            started: Instant::now(),
            context,
            year,
            search: None,
            work: None,
            queue: None,
            base: String::new(),
            edits: Vec::new(),
            evaluated: false,
        })
    }

    /// Whether nothing of this conversion can be shown without a score. A
    /// calendar year needs none.
    pub(super) fn needs_model(&self) -> bool {
        self.year.is_none()
    }

    // Past this point, changes already chosen are shown without further scoring.
    fn late(&self) -> bool {
        self.started.elapsed() > Duration::from_millis(SCAN_APPLY_MS)
    }

    pub(super) fn cancel(self, llm: Option<&dyn Rerank>) {
        if let (Some((ticket, _)), Some(llm)) = (self.work, llm) {
            llm.cancel(ticket);
        }
    }
}

// The inflection data covers single-kanji stems with the same kana ending.
fn ending(surface: &str) -> Option<&str> {
    let split = surface
        .trim_end_matches(|c| ('ぁ'..='ゖ').contains(&c))
        .len();
    let (stem, kana) = surface.split_at(split);
    let mut chars = stem.chars();
    let single = chars.next().is_some_and(|c| ('一'..='鿿').contains(&c)) && chars.next().is_none();
    (single && kana.chars().count() >= 2).then_some(kana)
}

fn key(special: SpecialKey) -> Key {
    Key {
        special: special as i32,
        ..Default::default()
    }
}

struct Ending<'a> {
    prefix: &'a [Segment],
    displayed: &'a str,
    inflection: &'a str,
    attached: &'static str,
    mark: &'static str,
}

impl<'a> Ending<'a> {
    fn parse(parts: &'a [Segment]) -> Option<Self> {
        let (last, prefix) = parts.split_last()?;
        let mark = ["、", "。"]
            .into_iter()
            .find(|mark| last.reading.ends_with(mark))
            .unwrap_or("");
        let (verb, prefix, attached) = if !mark.is_empty() && last.reading == mark {
            if last.value != mark {
                return None;
            }
            let (verb, prefix) = prefix.split_last()?;
            (verb, prefix, "")
        } else {
            (last, prefix, mark)
        };
        let reading = verb.reading.strip_suffix(attached)?;
        let displayed = verb.value.strip_suffix(attached)?;
        if !(1..=3).contains(&prefix.len())
            || reading.contains(['、', '。'])
            || prefix
                .iter()
                .any(|part| part.reading.contains(['、', '。']))
        {
            return None;
        }
        Some(Self {
            prefix,
            displayed,
            inflection: ending(displayed)?,
            attached,
            mark,
        })
    }

    fn proposal(
        &self,
        last: &[FocusCandidate],
        mut job: Job,
        policy: &Preview,
    ) -> Option<Proposal> {
        job.reading = job.reading.strip_suffix(self.mark)?.to_owned();
        job.prefix = self.prefix.iter().map(|s| s.value.as_str()).collect();
        job.candidates.push(self.displayed.to_owned());
        for c in last.iter().take(limits::CANDIDATES) {
            let Some(value) = c.value.strip_suffix(self.attached) else {
                continue;
            };
            if !c.transliteration
                && ending(value) == Some(self.inflection)
                && !job.candidates.iter().any(|v| v == value)
                && job.candidates.len() < limits::CANDIDATES
            {
                job.candidates.push(value.to_owned());
            }
        }
        job.valid().then(|| Proposal {
            before: String::new(),
            after: self.mark.to_owned(),
            penalties: vec![0.; job.candidates.len()],
            original_count: job.candidates.len(),
            changes: vec![None; job.candidates.len()],
            numbers: Vec::new(),
            references: Vec::new(),
            undecided: Vec::new(),
            job,
            margin: policy.ending,
            numerals: false,
            lattice: None,
            round: 1,
            policy: *policy,
        })
    }
}

// Mozc can offer a reading in kana for an entire name, or omit the dots in
// a transliterated name. Those are useful manual choices even when a small
// language model strongly prefers the more common spelling. Kanji in a text
// with `katakana` or more katakana characters, as beside a transliterated
// name, are kept as well; zero leaves them to the model.
fn preserves_name_spelling(original: &str, candidate: &str, katakana_limit: u8) -> bool {
    let original: Vec<char> = original.chars().collect();
    let candidate: Vec<char> = candidate.chars().collect();
    let (old, new) = differing(&original, &candidate);
    let old_kanji = kanji_count(old);
    let old_katakana = katakana_count(old);
    !(old.contains(&'・')
        || new.contains(&'・')
        || (old_kanji >= 2 && kanji_count(new) < old_kanji)
        || (old_katakana >= 3 && katakana_count(new) < old_katakana)
        || beside_katakana(&original, old, katakana_limit))
}

// A number read across segments replaces words that were never a name, so
// fewer kanji or katakana are no sign of one. Only the amount of katakana
// beside which the weights keep kanji applies.
fn number_is_trusted(original: &str, candidate: &str, katakana_limit: u8) -> bool {
    let original: Vec<char> = original.chars().collect();
    let candidate: Vec<char> = candidate.chars().collect();
    let (old, _) = differing(&original, &candidate);
    !beside_katakana(&original, old, katakana_limit)
}

// The characters of two texts between their common beginning and end.
fn differing<'a>(original: &'a [char], candidate: &'a [char]) -> (&'a [char], &'a [char]) {
    let prefix = original
        .iter()
        .zip(candidate)
        .take_while(|(a, b)| a == b)
        .count();
    let left = &original[prefix..];
    let right = &candidate[prefix..];
    let suffix = left
        .iter()
        .rev()
        .zip(right.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    (&left[..left.len() - suffix], &right[..right.len() - suffix])
}

fn kanji_count(text: &[char]) -> usize {
    text.iter().filter(|&&c| ('一'..='鿿').contains(&c)).count()
}

fn katakana_count(text: &[char]) -> usize {
    text.iter()
        .filter(|&&c| matches!(c, 'ァ'..='ヺ' | 'ー'))
        .count()
}

// Whether `old`, a part of `original`, has kanji that the weights keep
// because the text holds `limit` or more katakana characters.
fn beside_katakana(original: &[char], old: &[char], limit: u8) -> bool {
    kanji_count(old) > 0 && limit > 0 && katakana_count(original) >= usize::from(limit)
}

// `spoken` are the numbers read across `parts`, as found in their whole
// clause. `before` and `after` are the texts displayed around `parts`.
fn contextual_proposal(
    parts: &[Segment],
    lists: &[Vec<FocusCandidate>],
    mut job: Job,
    policy: &Preview,
    spoken: &[spoken::Number],
    before: &str,
    after: &str,
) -> Proposal {
    job.candidates
        .push(parts.iter().map(|s| s.value.as_str()).collect());
    let mut penalties = vec![0.0];
    let mut changes = vec![Some(Vec::new())];
    // Reserve at most two slots for source-confirmed alternatives. They still
    // need a positive model score lead and pass the normal preview checks.
    for (pos, part) in parts.iter().enumerate() {
        if let Some((spelling, support)) = search::teacher_spelling(&part.reading, &part.value) {
            let value = search::substitute(parts, pos, &spelling);
            if value.chars().count() <= limits::CANDIDATE_CHARS
                && preserves_name_spelling(&job.candidates[0], &value, policy.katakana)
                && !job.candidates.contains(&value)
            {
                job.candidates.push(value);
                penalties.push(if support >= 5 { -3.5 } else { -2.5 });
                changes.push(Some(vec![(pos, spelling)]));
                if job.candidates.len() == 3 {
                    break;
                }
            }
        }
    }
    // A number that the converter read as separate words takes the next
    // slots, with kanji numerals and with digits, so that both are scored.
    let mut numbers = Vec::new();
    // The two spellings of each of them, and whether the kanji numerals took
    // a slot of their own. Their places are known once every slot is taken.
    let mut twins: Vec<([Option<String>; 2], bool)> = Vec::new();
    for number in spoken.iter().take(2) {
        let mut texts = [None, None];
        let mut own = false;
        for (slot, spelling) in number.spellings().iter().enumerate() {
            let value: String = parts[..number.range.start]
                .iter()
                .map(|s| s.value.clone())
                .chain([number.written(spelling)])
                .chain(parts[number.range.end..].iter().map(|s| s.value.clone()))
                .collect();
            let offered = match job.candidates.iter().position(|c| *c == value) {
                Some(index) => index > 0,
                None => {
                    let added = value.chars().count() <= limits::CANDIDATE_CHARS
                        && number_is_trusted(&job.candidates[0], &value, policy.katakana);
                    if added {
                        job.candidates.push(value.clone());
                        penalties.push(policy.rank);
                        changes.push(None);
                        own |= slot == 0;
                    }
                    added
                }
            };
            texts[slot] = offered.then_some(value);
        }
        if texts.iter().any(Option::is_some) {
            let width =
                |segments: &[Segment]| -> usize { segments.iter().map(|s| s.value.len()).sum() };
            let from = width(&parts[..number.range.start]);
            let to = from + width(&parts[number.range.clone()]);
            numbers.push(Spoken {
                number: number.clone(),
                kanji: None,
                digits: None,
                shown_digits: numerals::digits_within(&job.candidates[0], after, from..to),
            });
            twins.push((texts, own));
        }
    }
    // Take the next distinct alternative at each position before exploring lower ranks.
    let mut later = 0;
    for rank in 1..=3 {
        for (pos, list) in lists.iter().enumerate() {
            if let Some(c) = list.get(rank)
                && !c.transliteration
            {
                let value: String = parts
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        if i == pos {
                            c.value.as_str()
                        } else {
                            s.value.as_str()
                        }
                    })
                    .collect();
                if value.chars().count() <= limits::CANDIDATE_CHARS
                    && !job.candidates.contains(&value)
                {
                    job.candidates.push(value);
                    penalties.push(rank as f32 * policy.rank);
                    changes.push(Some(vec![(pos, c.value.clone())]));
                    later += usize::from(rank > 1);
                    if job.candidates.len() == limits::CANDIDATES {
                        break;
                    }
                }
            }
        }
        if job.candidates.len() == limits::CANDIDATES {
            break;
        }
    }
    // What the digits of a number are compared with has to be scored: the
    // display with its own numerals in digits. Unless it is a candidate
    // already, it takes one of the nine places that are always scored: a
    // free one, or that of a second or third alternative, which moves back.
    // The first alternative of every segment keeps its place. Where no other
    // place is left, the number's kanji numerals give theirs. The number is
    // then compared in digits, and its notation afterwards. Digits that the
    // safeguards refuse are not worth the place.
    let whole = |text: &str| format!("{before}{}{text}{after}", job.prefix);
    let mut room = limits::CANDIDATES - job.candidates.len() + later;
    let mut shown: Vec<String> = Vec::new();
    let mut behind: Vec<String> = Vec::new();
    for (spoken, (texts, own)) in numbers.iter().zip(&twins) {
        let (Some(digits), Some(text)) = (&texts[1], &spoken.shown_digits) else {
            continue;
        };
        if text.chars().count() > limits::CANDIDATE_CHARS
            || job.candidates.contains(text)
            || shown.contains(text)
        {
            continue;
        }
        if room > 0 {
            room -= 1;
        } else if *own
            && (job.context.is_empty()
                || number_is_trusted(&whole(&job.candidates[0]), &whole(digits), policy.katakana))
        {
            behind.extend(texts[0].clone());
        } else {
            continue;
        }
        shown.push(text.clone());
    }
    let mut moved = Vec::new();
    for text in &behind {
        if let Some(at) = job.candidates.iter().position(|c| c == text) {
            moved.push((
                job.candidates.remove(at),
                penalties.remove(at),
                changes.remove(at),
            ));
        }
    }
    let at = job
        .candidates
        .len()
        .min(limits::CANDIDATES.saturating_sub(shown.len()));
    for (offset, text) in shown.iter().enumerate() {
        job.candidates.insert(at + offset, text.clone());
        penalties.insert(at + offset, 0.0);
        changes.insert(at + offset, None);
    }
    let at = job.candidates.len().min(limits::CANDIDATES);
    for (offset, (text, penalty, change)) in moved.into_iter().enumerate() {
        job.candidates.insert(at + offset, text);
        penalties.insert(at + offset, penalty);
        changes.insert(at + offset, change);
    }
    let place = |text: &String| job.candidates.iter().position(|c| c == text);
    let references: Vec<_> = shown.iter().filter_map(place).collect();
    for (spoken, (texts, _)) in numbers.iter_mut().zip(&twins) {
        spoken.kanji = texts[0].as_ref().and_then(place);
        spoken.digits = texts[1].as_ref().and_then(place);
    }
    let original_count = job.candidates.len().min(limits::CANDIDATES);
    let mut p = Proposal {
        job,
        before: String::new(),
        after: String::new(),
        penalties,
        margin: policy.sentence,
        original_count,
        numerals: false,
        changes,
        numbers,
        references,
        undecided: Vec::new(),
        lattice: None,
        round: 1,
        policy: *policy,
    };
    search::widen(&mut p, parts, lists);
    p
}

// Score a clause in windows within the model's limits, the last window first.
// The text displayed before a window supplies the same left context as
// preceding text; the text after it is kept as displayed.
fn scored_windows(
    parts: &[Segment],
    lists: &[Vec<FocusCandidate>],
    job: &Job,
    limit: usize,
    policy: &Preview,
    explore: impl FnOnce(&[Segment], &mut Proposal) -> Vec<search::Split>,
) -> Vec<Proposal> {
    let mut found = Vec::new();
    let mut explore = Some(explore);
    // Numbers that the converter read as separate words. The whole clause is
    // read for them, and each is scored whole.
    let numbers = spoken::numbers(parts, lists);
    // Without preceding text, the first segment has nothing to its left.
    let first = usize::from(job.context.is_empty());
    let mut end = parts.len();
    while end > first && found.len() < limit {
        let mut start = end;
        let mut length = 0;
        while start > first && end - start < 8 {
            let chars = parts[start - 1].value.chars().count();
            if length + chars > limits::CANDIDATE_CHARS {
                break;
            }
            length += chars;
            start -= 1;
        }
        if start == end {
            // A single segment exceeds the limit. Leave it as displayed.
            end -= 1;
            continue;
        }
        // A window does not begin inside a number: the next window takes it.
        let cut = |number: &&spoken::Number| {
            let range = &number.range;
            range.start < start && start < range.end && range.end < end
        };
        if let Some(number) = numbers.iter().find(cut) {
            start = number.range.end;
        }
        let window = &parts[start..end];
        let choices = &lists[start..end];
        let before: String = parts[..start].iter().map(|s| s.value.as_str()).collect();
        let mut job = job.clone();
        if window.len() < parts.len() {
            let reading: String = window.iter().map(|s| s.reading.as_str()).collect();
            job.reading = kanade_llm::katakana(&reading);
        }
        if start > 0 {
            let Some(context) = kanade_llm::context_window(&format!("{}{before}", job.context))
            else {
                end = start;
                continue;
            };
            job.context = context;
        }
        let inside: Vec<_> = numbers
            .iter()
            .filter(|number| start <= number.range.start && number.range.end <= end)
            .map(|number| {
                let mut number = number.clone();
                number.range = number.range.start - start..number.range.end - start;
                number
            })
            .collect();
        let after: String = parts[end..].iter().map(|s| s.value.as_str()).collect();
        let mut item = contextual_proposal(window, choices, job, policy, &inside, &before, &after);
        let splits = explore
            .take()
            .map_or_else(Vec::new, |probe| probe(window, &mut item));
        search::paired(&mut item, window, choices);
        search::extend_splits(&mut item, window, choices, &splits);
        item.before = before;
        item.after = after;
        item.lattice = Some(Lattice {
            parts: window.to_vec(),
            lists: choices.to_vec(),
            numbers: inside,
        });
        if item.job.valid_for(true) {
            found.push(item);
        }
        end = start;
    }
    found
}

fn with_years(
    parts: &[Segment],
    lists: &[Vec<FocusCandidate>],
    spans: &[(usize, usize, String)],
) -> (Vec<Segment>, Vec<Vec<FocusCandidate>>) {
    let mut segments = Vec::new();
    let mut choices = Vec::new();
    let mut at = 0;
    for (start, end, year) in spans {
        segments.extend_from_slice(&parts[at..*start]);
        choices.extend_from_slice(&lists[at..*start]);
        segments.push(Segment {
            reading: parts[*start..*end]
                .iter()
                .map(|s| s.reading.as_str())
                .collect(),
            value: year.clone(),
            ..Default::default()
        });
        choices.push(vec![FocusCandidate {
            reading: String::new(),
            annotation: String::new(),
            value: year.clone(),
            transliteration: false,
            history_deletable: false,
        }]);
        at = *end;
    }
    segments.extend_from_slice(&parts[at..]);
    choices.extend_from_slice(&lists[at..]);
    (segments, choices)
}

// Keep the typing session untouched. Check the reconstructed segmentation before using each list.
// With `years`, calendar years written across segments are replaced before scoring.
fn alternatives<C: ConversionEngine>(
    backend: &mut C,
    original: &Preedit,
    context: &str,
    years: bool,
    policy: &Preview,
) -> Option<Vec<Proposal>> {
    let parts = &original.segments;
    let whole = reading(original);
    let ending = if context.is_empty() && !years {
        Ending::parse(parts)
    } else {
        None
    };
    let split_deadline = Instant::now() + Duration::from_millis(80);
    let probe = backend.create_session().ok()?;
    let collect = |backend: &mut C| -> Option<Vec<Proposal>> {
        backend.restore(probe, &whole).ok()?;
        let mut snap = backend.key(probe, &key(SpecialKey::Space), context).ok()?;
        let mut lists = Vec::with_capacity(parts.len());
        let job = Job {
            context: context.into(),
            reading: kanade_llm::katakana(&whole),
            prefix: String::new(),
            suffix: String::new(),
            candidates: Vec::new(),
        };
        for pos in 0..parts.len() {
            if !snap.converting
                || snap.commit.is_some()
                || snap.focused_segment != pos
                || !snap
                    .preedit
                    .segments
                    .iter()
                    .map(|s| &s.reading)
                    .eq(parts.iter().map(|s| &s.reading))
            {
                return None;
            }
            if let Some(ending) = &ending {
                if pos == ending.prefix.len() {
                    return ending
                        .proposal(&snap.candidates, job, policy)
                        .map(|p| vec![p]);
                }
            } else {
                // The disposable profile has no conversion history. Anchor the
                // comparison to the visible spelling, even if it ranks lower here.
                let displayed = snap
                    .candidates
                    .iter()
                    .position(|c| c.value == parts[pos].value)?;
                snap.candidates[..=displayed].rotate_right(1);
                let mut list = Vec::<FocusCandidate>::new();
                for c in &snap.candidates {
                    if !list.iter().any(|other| other.value == c.value) {
                        list.push(c.clone());
                    }
                    if list.len() == 32 {
                        break;
                    }
                }
                lists.push(list);
            }
            if pos + 1 < parts.len() {
                snap = backend.key(probe, &key(SpecialKey::Right), context).ok()?;
            }
        }
        // A calendar year takes the place of the segments that spell it. The
        // other segments are scored around it; the year itself has one spelling.
        let dated = years
            .then(|| calendar::years(parts, context))
            .flatten()
            .map(|spans| with_years(parts, &lists, &spans));
        let (parts, lists) = match &dated {
            Some((parts, lists)) => (parts.as_slice(), lists.as_slice()),
            None => (parts.as_slice(), lists.as_slice()),
        };
        let found = if whole.contains(['、', '。']) {
            clause::proposals(parts, lists, &job, policy, |segments, p| {
                search::resegment(backend, probe, segments, p, split_deadline)
            })
        } else {
            scored_windows(parts, lists, &job, MAX_WINDOWS, policy, |segments, p| {
                search::resegment(backend, probe, segments, p, split_deadline)
            })
        };
        (!found.is_empty()).then_some(found)
    };
    let result = collect(backend);
    backend.delete_session(probe).ok()?;
    result
}

impl<C: ConversionEngine> Engine<C> {
    pub(super) fn poll_sentence(
        &mut self,
        session: &mut Session,
        poll: &CandidatePoll,
    ) -> Result<Option<Response>, Code> {
        let revision = session.revision;
        let status = |pending, outcome| {
            Ok(Some(Response {
                body: Some(response::Body::Candidates(CandidateUpdate {
                    revision: Some(revision),
                    candidates: None,
                    pending,
                    refinement_outcome: outcome as i32,
                })),
            }))
        };
        let Some(seq) = poll.preview_event_seq else {
            return self.poll(session, poll);
        };
        if poll.revision != Some(revision) || poll.candidate_revision != session.candidate_revision
        {
            return status(false, RefinementOutcome::Unspecified);
        }
        if session.pending.is_some() {
            return status(true, RefinementOutcome::Unspecified);
        }
        check_revision(session, poll.revision.as_ref(), seq)?;
        let mut pending = session.sentence.take().ok_or(Code::Invalid)?;
        let llm = self.llm.as_deref();
        if session.manual
            || !session.snapshot.converting
            || !self.settings.llm
            || session.privacy != Privacy::Normal
            || !self.settings.learning_allowed(&session.app)
        {
            pending.cancel(llm);
            return status(false, RefinementOutcome::Unspecified);
        }
        if pending.started.elapsed() > Duration::from_millis(limits::RESULT_TTL_MS) {
            pending.cancel(llm);
            return status(false, RefinementOutcome::TimedOut);
        }
        let Some(llm) = llm else {
            return status(false, RefinementOutcome::Unavailable);
        };
        if pending.year.is_some() && !self.settings.calculated_dates {
            return status(false, RefinementOutcome::Unspecified);
        }
        let scan = Self::scan(
            &mut self.probe,
            &mut self.backend,
            &self.preview,
            llm,
            &session.snapshot.preedit,
            &mut pending,
        );
        let (scored, outcome) = match scan {
            Scan::Waiting => {
                session.sentence = Some(pending);
                return status(true, RefinementOutcome::Unspecified);
            }
            Scan::Finished(scored, outcome) => (scored, outcome),
        };
        // A calendar year needs no model. Show it even when nothing was scored.
        let digits = |text: String| contextual_digits(&text, &pending.context).unwrap_or(text);
        let year = pending.year.take().map(digits);
        let Some(value) = scored.map(digits).or_else(|| year.clone()) else {
            return status(false, outcome);
        };
        if value == surface(&session.snapshot.preedit) {
            return status(false, RefinementOutcome::Evaluated);
        }
        let source = if year.as_ref() == Some(&value) {
            CandidateSource::Date
        } else {
            CandidateSource::Llm
        };
        self.apply_sentence(session, seq, poll, value, source)
    }

    // Score one window at a time. Each keeps the text displayed around it, so
    // the accepted changes are independent and are shown together.
    fn scan(
        probe: &mut Option<probe::Worker>,
        backend: &mut C,
        policy: &Preview,
        llm: &dyn Rerank,
        preedit: &Preedit,
        pending: &mut Pending,
    ) -> Scan {
        if pending.queue.is_none() {
            let years = pending.year.is_some();
            let proposals = if let Some(worker) = probe {
                if pending.search.is_none() {
                    let expires = pending.started + Duration::from_millis(limits::RESULT_TTL_MS);
                    pending.search =
                        worker.submit(preedit, &pending.context, years, policy, expires);
                }
                let Some(ticket) = pending.search.as_ref() else {
                    return Scan::Finished(None, RefinementOutcome::Unavailable);
                };
                match ticket.poll() {
                    // A calendar year is not held back by a slow search.
                    std::task::Poll::Pending if years && pending.late() => {
                        return Scan::Finished(None, RefinementOutcome::TimedOut);
                    }
                    std::task::Poll::Pending => return Scan::Waiting,
                    std::task::Poll::Ready(Ok(proposals)) => proposals,
                    std::task::Poll::Ready(Err(outcome)) => return Scan::Finished(None, outcome),
                }
            } else {
                alternatives(backend, preedit, &pending.context, years, policy)
            };
            let Some(mut proposals) = proposals.filter(|found| !found.is_empty()) else {
                return Scan::Finished(None, RefinementOutcome::Unspecified);
            };
            pending.search = None;
            pending.base = proposals[0].render(0);
            proposals.reverse();
            pending.queue = Some(proposals);
        }
        loop {
            let (ticket, proposal) = match pending.work.take() {
                Some(work) => work,
                None => {
                    let early = pending.started.elapsed() <= Duration::from_millis(SCAN_START_MS);
                    let next = if early || !pending.evaluated {
                        pending.queue.as_mut().and_then(Vec::pop)
                    } else {
                        None
                    };
                    let Some(proposal) = next else {
                        break;
                    };
                    let ticket = if proposal.job.context.is_empty() {
                        llm.submit(proposal.job.clone())
                    } else {
                        llm.submit_sentence(proposal.job.clone())
                    };
                    let Some(ticket) = ticket else {
                        break;
                    };
                    (ticket, proposal)
                }
            };
            match llm.take(ticket) {
                Poll::Pending => {
                    // Show what is already decided rather than lose it to a slow job.
                    if (!pending.edits.is_empty() || pending.year.is_some()) && pending.late() {
                        llm.cancel(ticket);
                        break;
                    }
                    pending.work = Some((ticket, proposal));
                    return Scan::Waiting;
                }
                Poll::Gone => break,
                Poll::Ready(scores) => {
                    let Some(winner) = proposal.winner(&scores, &pending.context) else {
                        continue;
                    };
                    pending.evaluated = true;
                    let start = proposal.before.len();
                    let end = pending.base.len() - proposal.after.len();
                    if winner != 0 {
                        // The numeral choice replaces the spelling chosen for its window.
                        pending.edits.retain(|edit| edit.0 != start);
                        let value =
                            format!("{}{}", proposal.job.prefix, proposal.job.candidates[winner]);
                        pending.edits.push((start, end, value));
                    }
                    // Settle the words of this window first, then its numbers.
                    let next = proposal
                        .next_round(winner)
                        .or_else(|| proposal.numeral_choice(winner, &pending.context));
                    if let Some(next) = next
                        && let Some(queue) = pending.queue.as_mut()
                    {
                        queue.push(next);
                    }
                }
            }
        }
        if !pending.evaluated {
            return Scan::Finished(None, refine::unscored(Some(llm)));
        }
        Scan::Finished(
            Some(compose(&pending.base, &pending.edits)),
            RefinementOutcome::Evaluated,
        )
    }

    fn apply_sentence(
        &mut self,
        session: &mut Session,
        seq: u64,
        poll: &CandidatePoll,
        value: String,
        source: CandidateSource,
    ) -> Result<Option<Response>, Code> {
        refine::cancel(self.llm.as_deref(), session);
        let choice =
            candidates::Choice::sentence(value, reading(&session.snapshot.preedit), source);
        let preview = self.choose(session, &choice, false)?;
        if source == CandidateSource::Llm && session.snapshot.preedit.segments.len() == 1 {
            session
                .choices
                .retain(|c| c.source != CandidateSource::Mozc || c.value != choice.value);
        }
        session.choices.insert(0, choice);
        session.selected_choice = 0;
        session.automatic = true;
        let mut reply = self.reply(
            session,
            seq,
            preview,
            session.preceding.clone(),
            session.privacy,
            true,
        )?;
        if let Some(page) = &mut reply.candidates {
            page.refinement_outcome = if source == CandidateSource::Llm {
                RefinementOutcome::Evaluated
            } else {
                RefinementOutcome::Unspecified
            } as i32;
        }
        session.last_request = Some(Request {
            body: Some(request::Body::Poll(*poll)),
        });
        session.last_reply = Some(reply.clone());
        Ok(Some(key_response(reply)))
    }
}

#[cfg(test)]
mod tests {
    use super::preserves_name_spelling;

    #[test]
    fn a_long_clause_is_scored_in_consecutive_windows() {
        use super::{FocusCandidate, Job, MAX_WINDOWS, Segment, scored_windows};
        let policy = &super::TEST_POLICY;
        let option = |value: &str| FocusCandidate {
            reading: String::new(),
            annotation: String::new(),
            value: value.into(),
            transliteration: false,
            history_deletable: false,
        };
        // Ten segments of four characters: eight fit one window.
        let parts: Vec<_> = (0..10)
            .map(|i| Segment {
                reading: "かきくけ".into(),
                value: format!("書き区{i}"),
                ..Default::default()
            })
            .collect();
        let lists: Vec<_> = parts
            .iter()
            .enumerate()
            .map(|(i, p)| vec![option(&p.value), option(&format!("柿苦{i}"))])
            .collect();
        let job = Job {
            context: "前の文。".into(),
            reading: "カキクケ".repeat(10),
            prefix: String::new(),
            suffix: String::new(),
            candidates: vec![],
        };
        let whole: String = parts.iter().map(|p| p.value.as_str()).collect();
        let found = scored_windows(&parts, &lists, &job, MAX_WINDOWS, policy, |_, _| Vec::new());
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|item| item.render(0) == whole));
        // The last eight segments come first, with the first two as their context.
        assert_eq!(found[0].before, "書き区0書き区1");
        assert_eq!(found[0].after, "");
        assert_eq!(found[0].job.context, "前の文。書き区0書き区1");
        assert_eq!(found[0].job.reading, "カキクケ".repeat(8));
        assert_eq!(found[1].before, "");
        assert_eq!(found[1].after, whole["書き区0書き区1".len()..]);
        assert_eq!(found[1].job.context, "前の文。");
        assert_eq!(found[1].job.reading, "カキクケ".repeat(2));
        assert!(found[1].job.candidates.contains(&"柿苦0書き区1".to_owned()));
        // Without preceding text the first segment is context only.
        let alone = Job {
            context: String::new(),
            ..job.clone()
        };
        let found = scored_windows(&parts, &lists, &alone, MAX_WINDOWS, policy, |_, _| {
            Vec::new()
        });
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].before, "書き区0");
        assert_eq!(found[1].job.context, "書き区0");
        assert_eq!(found[1].job.candidates[0], "書き区1");
        assert_eq!(
            scored_windows(&parts, &lists, &job, 1, policy, |_, _| Vec::new()).len(),
            1
        );
    }

    #[test]
    fn accepted_windows_are_combined_and_weak_leads_keep_the_display() {
        use super::{Job, Proposal, compose};
        let base = "人気は、二年とする。";
        let edits = [
            (
                "人気は、".len(),
                "人気は、二年とする".len(),
                "2年とする".to_owned(),
            ),
            (0, "人気は".len(), "任期は".to_owned()),
        ];
        assert_eq!(compose(base, &edits), "任期は、2年とする。");
        assert_eq!(compose(base, &[]), base);
        let proposal = Proposal {
            job: Job {
                context: "委員の".into(),
                reading: "ニンキハ".into(),
                prefix: String::new(),
                suffix: String::new(),
                candidates: vec!["人気は".into(), "任期は".into(), "人気派".into()],
            },
            before: String::new(),
            after: "、二年とする。".into(),
            penalties: vec![0.0, 0.75, 1.5],
            margin: 5.0,
            original_count: 3,
            numerals: false,
            changes: vec![None; 3],
            numbers: Vec::new(),
            references: Vec::new(),
            undecided: Vec::new(),
            lattice: None,
            round: 1,
            policy: super::TEST_POLICY,
        };
        let winner = |scores: &[Option<f32>]| proposal.winner(scores, "委員の");
        // A lead of five over the display and 3.75 over the runner-up, after penalties.
        assert_eq!(winner(&[Some(-20.0), Some(-14.0), Some(-30.0)]), Some(1));
        assert_eq!(winner(&[Some(-20.0), Some(-14.5), Some(-30.0)]), Some(0));
        assert_eq!(winner(&[Some(-20.0), Some(-14.0), Some(-16.0)]), Some(0));
        assert_eq!(winner(&[Some(-20.0), None, Some(-30.0)]), None);
        assert_eq!(winner(&[Some(-20.0), Some(-14.0)]), None);
    }

    #[test]
    fn decimal_width_follows_unambiguous_committed_digits() {
        use super::contextual_digits;
        let surface = "第１２回の３日目、二人でＡ組へ";
        assert_eq!(
            contextual_digits(surface, "前回は9日間。").as_deref(),
            Some("第12回の3日目、二人でＡ組へ")
        );
        for context in ["", "第九回。", "第９回。", "9月１０日。"] {
            assert_eq!(contextual_digits(surface, context), None);
        }
        assert_eq!(contextual_digits("三人で12回", "9月に"), None);
        // A year in digits is evidence from the conversion itself.
        assert_eq!(
            contextual_digits("1993年３月場所に", "").as_deref(),
            Some("1993年3月場所に")
        );
        assert_eq!(contextual_digits("1993年３月場所に", "第９回。"), None);
    }

    #[test]
    fn numbers_are_compared_after_the_words_and_follow_committed_digits() {
        use super::{Job, Proposal};
        let policy = super::TEST_POLICY;
        let words = Proposal {
            job: Job {
                context: "2010年には".into(),
                reading: "ニイダッタ".into(),
                prefix: String::new(),
                suffix: String::new(),
                candidates: vec!["二位だった".into(), "二医だった".into()],
            },
            before: "同じく".into(),
            after: "場合には".into(),
            penalties: vec![0.0, 0.75],
            margin: 5.0,
            original_count: 2,
            numerals: false,
            changes: vec![None; 2],
            numbers: Vec::new(),
            references: Vec::new(),
            undecided: Vec::new(),
            lattice: None,
            round: 1,
            policy,
        };
        let choice = words.numeral_choice(0, "2010年には").unwrap();
        assert!(choice.numerals);
        assert_eq!(choice.job.candidates, ["二位だった", "2位だった"]);
        assert_eq!(
            (choice.before.as_str(), choice.after.as_str()),
            ("同じく", "場合には")
        );
        // Committed digits settle a tie; the kanji numeral stays when it scores higher.
        assert_eq!(choice.margin, policy.digits_seen);
        assert_eq!(choice.winner(&[Some(-8.0), Some(-8.0)], ""), Some(1));
        assert_eq!(choice.winner(&[Some(-8.0), Some(-8.5)], ""), Some(0));
        // Without them the digits need a clear lead.
        let plain = words.numeral_choice(0, "前の文。").unwrap();
        assert_eq!(plain.margin, policy.digits);
        assert_eq!(
            plain.winner(&[Some(-8.0), Some(-8.0 + policy.digits - 0.5)], ""),
            Some(0)
        );
        assert_eq!(
            plain.winner(&[Some(-8.0), Some(-8.0 + policy.digits)], ""),
            Some(1)
        );
        // A word without a number, and a choice that is already about numbers.
        assert!(words.numeral_choice(1, "2010年には").is_none());
        assert!(choice.numeral_choice(1, "2010年には").is_none());
    }

    #[test]
    fn an_accepted_correction_is_followed_by_one_more_look_at_its_window() {
        use super::{FocusCandidate, Job, MAX_WINDOWS, Preview, Segment, scored_windows};
        // A model whose registration allows a second accepted spelling per window.
        let policy = &Preview {
            rounds: 2,
            ..super::TEST_POLICY
        };
        let option = |value: &str| FocusCandidate {
            reading: String::new(),
            annotation: String::new(),
            value: value.into(),
            transliteration: false,
            history_deletable: false,
        };
        let parts: Vec<_> = [
            ("しんこうと", "進行と"),
            ("しぜん", "自然"),
            ("かんの", "感の"),
        ]
        .iter()
        .map(|(reading, value)| Segment {
            reading: (*reading).into(),
            value: (*value).into(),
            ..Default::default()
        })
        .collect();
        let lists = [
            vec![option("進行と"), option("信仰と"), option("侵攻と")],
            vec![option("自然")],
            vec![option("感の"), option("観の"), option("間の")],
        ];
        let job = Job {
            context: "古代の".into(),
            reading: "シンコウトシゼンカンノ".into(),
            prefix: String::new(),
            suffix: String::new(),
            candidates: vec![],
        };
        let found = scored_windows(&parts, &lists, &job, MAX_WINDOWS, policy, |_, _| Vec::new());
        let first = &found[0];
        let accepted = "信仰と自然感の".to_owned();
        let index = first
            .job
            .candidates
            .iter()
            .position(|c| *c == accepted)
            .unwrap();
        // The accepted spelling becomes the display that the next candidates change.
        let second = first.next_round(index).unwrap();
        assert_eq!((first.round, second.round), (1, 2));
        assert_eq!(second.job.candidates[0], accepted);
        assert!(second.job.candidates.contains(&"信仰と自然観の".to_owned()));
        assert_eq!(second.job.context, first.job.context);
        assert_eq!(second.job.reading, first.job.reading);
        assert_eq!(second.render(0), accepted);
        // No further look after two accepted spellings, or when nothing changed.
        assert!(second.next_round(1).is_none());
        assert!(first.next_round(0).is_none());
    }

    #[test]
    fn a_spoken_number_is_one_alternative_in_two_notations() {
        use super::{FocusCandidate, Job, MAX_WINDOWS, Preview, Segment, scored_windows};
        let policy = &super::TEST_POLICY;
        let option = |value: &str| FocusCandidate {
            reading: String::new(),
            annotation: String::new(),
            value: value.into(),
            transliteration: false,
            history_deletable: false,
        };
        let segments = |words: &[(&str, &str)]| -> Vec<Segment> {
            words
                .iter()
                .map(|(reading, value)| Segment {
                    reading: (*reading).into(),
                    value: (*value).into(),
                    ..Default::default()
                })
                .collect()
        };
        let parts = segments(&[
            ("せんしゅは", "選手は"),
            ("はち", "八"),
            ("いち", "市"),
            ("しあい", "試合"),
        ]);
        let lists = [
            vec![option("選手は")],
            vec![option("八"), option("鉢")],
            vec![option("市"), option("一"), option("位置")],
            vec![option("試合"), option("仕合")],
        ];
        let job = Job {
            context: "昨季の".into(),
            reading: "センシュハハチイチシアイ".into(),
            prefix: String::new(),
            suffix: String::new(),
            candidates: vec![],
        };
        let windows = |policy: &Preview| {
            scored_windows(&parts, &lists, &job, MAX_WINDOWS, policy, |_, _| Vec::new())
        };
        let found = windows(policy);
        let first = &found[0];
        // After the displayed text come the number's kanji numerals and its digits.
        assert_eq!(
            first.job.candidates[..3],
            ["選手は八市試合", "選手は八一試合", "選手は81試合"]
        );
        assert!(first.original_count > 2);
        let scores = |kanji: f32, digits: f32| {
            let mut scores = vec![Some(-60.0); first.job.candidates.len()];
            scores[..3].copy_from_slice(&[Some(-40.0), Some(kanji), Some(digits)]);
            scores
        };
        // Close scores of the two notations do not hold the number back. The
        // digits stand for it with their lead over the kanji numerals.
        let led = scores(-10.0 - policy.digits, -10.0);
        assert_eq!(first.winner(&scores(-12.0, -10.0), ""), Some(1));
        assert_eq!(first.winner(&led, ""), Some(2));
        // Committed digits settle a tie.
        assert_eq!(
            first.winner(&scores(-10.0, -10.0), "昨季は30試合。"),
            Some(2)
        );
        // The better notation still needs the lead over the displayed words.
        assert_eq!(first.winner(&scores(-37.0, -36.0), ""), Some(0));
        // Two kanji become digits here, which would keep the display of a name.
        assert!(!preserves_name_spelling(
            &first.render(0),
            &first.render(2),
            policy.katakana
        ));

        // Weights that look at a window again settle its words around the
        // kanji numerals, although the digits lead.
        let again = Preview {
            rounds: 2,
            ..*policy
        };
        let found = windows(&again);
        let first = &found[0];
        assert_eq!(first.winner(&led, ""), Some(1));
        let second = first.next_round(1).unwrap();
        assert_eq!(second.job.candidates[0], "選手は八一試合");
        let lattice = second.lattice.as_ref().unwrap();
        let cut: Vec<_> = lattice
            .parts
            .iter()
            .map(|s| (s.reading.as_str(), s.value.as_str()))
            .collect();
        // The number is one segment with one spelling from here on.
        assert_eq!(
            cut,
            [
                ("せんしゅは", "選手は"),
                ("はちいち", "八一"),
                ("しあい", "試合")
            ]
        );
        assert!(second.numbers.is_empty());
        assert!(second.job.candidates.contains(&"選手は八一仕合".to_owned()));
        assert!(second.job.candidates.iter().all(|c| !c.contains("81")));
        // With the words settled, its notation is compared like that of every
        // number, although no counter follows it.
        assert_eq!(second.undecided, ["八一"]);
        let notation = second.numeral_choice(0, "").unwrap();
        assert_eq!(notation.job.candidates, ["選手は八一試合", "選手は81試合"]);

        // Weights that keep kanji beside a long katakana name keep these too.
        let parts = segments(&[
            ("ぶるーさんだーすの", "ブルーサンダースの"),
            ("はち", "八"),
            ("いち", "市"),
            ("しあい", "試合"),
        ]);
        let lists: Vec<_> = parts.iter().map(|p| vec![option(&p.value)]).collect();
        let job = Job {
            reading: "ブルーサンダースノハチイチシアイ".into(),
            ..job
        };
        let offered = |policy: &Preview| {
            let found =
                scored_windows(&parts, &lists, &job, MAX_WINDOWS, policy, |_, _| Vec::new());
            found.first().map_or(0, |item| item.numbers.len())
        };
        assert_eq!(offered(policy), 0);
        assert_eq!(
            offered(&Preview {
                katakana: 0,
                ..*policy
            }),
            1
        );
    }

    #[test]
    fn the_digits_of_a_number_are_compared_with_the_display_in_digits() {
        use super::{FocusCandidate, Job, MAX_WINDOWS, Segment, scored_windows};
        let policy = &super::TEST_POLICY;
        let option = |value: &str| FocusCandidate {
            reading: String::new(),
            annotation: String::new(),
            value: value.into(),
            transliteration: false,
            history_deletable: false,
        };
        // 四級, the fourth grade, reads like forty-nine.
        let parts: Vec<_> = [
            ("よん", "四"),
            ("きゅうから", "級から"),
            ("しょだんまで", "初段まで"),
        ]
        .iter()
        .map(|(reading, value)| Segment {
            reading: (*reading).into(),
            value: (*value).into(),
            ..Default::default()
        })
        .collect();
        let job = Job {
            context: "検定は".into(),
            reading: "ヨンキュウカラショダンマデ".into(),
            prefix: String::new(),
            suffix: String::new(),
            candidates: vec![],
        };
        // `digit` adds the converter's own candidate that writes 四 as 4.
        let windows = |digit: bool| {
            let mut grade = vec![option("四")];
            grade.extend(digit.then(|| option("4")));
            let lists = [
                grade,
                vec![option("級から"), option("球から")],
                vec![option("初段まで")],
            ];
            scored_windows(&parts, &lists, &job, MAX_WINDOWS, policy, |_, _| Vec::new())
        };
        let found = windows(true);
        let first = &found[0];
        let index = |text: &str| first.job.candidates.iter().position(|c| c == text).unwrap();
        let (kanji, digits) = (index("四九から初段まで"), index("49から初段まで"));
        let shown = index("4級から初段まで");
        assert_eq!(
            first.numbers[0].shown_digits.as_deref(),
            Some("4級から初段まで")
        );
        let scores = |number: f32, display: f32| {
            let mut scores = vec![Some(-60.0); first.job.candidates.len()];
            scores[0] = Some(-30.0);
            scores[kanji] = Some(-50.0);
            scores[digits] = Some(number);
            scores[shown] = Some(display);
            scores
        };
        // 49 leads 四級 by far, only because it has digits. It does not lead
        // 4級, the converter's own candidate, which is accepted as it was
        // without the number.
        assert!(first.references.is_empty());
        assert_eq!(first.winner(&scores(-10.0, -11.0), ""), Some(shown));
        // With the lead over 4級 as well, the number is read.
        assert_eq!(
            first.winner(&scores(-10.0, -10.0 - policy.sentence), ""),
            Some(digits)
        );

        // Where the converter does not offer 4級, it is scored all the same,
        // only to compare the digits with. It is never chosen itself.
        let found = windows(false);
        let first = &found[0];
        let index = |text: &str| first.job.candidates.iter().position(|c| c == text).unwrap();
        let (kanji, digits) = (index("四九から初段まで"), index("49から初段まで"));
        let shown = index("4級から初段まで");
        assert_eq!(first.references, [shown]);
        assert!(shown < first.original_count);
        let scores = |kanji_score: f32, display: f32| {
            let mut scores = vec![Some(-60.0); first.job.candidates.len()];
            scores[0] = Some(-30.0);
            scores[kanji] = Some(kanji_score);
            scores[digits] = Some(-10.0);
            scores[shown] = Some(display);
            scores
        };
        assert_eq!(first.winner(&scores(-50.0, -11.0), ""), Some(0));
        assert_eq!(
            first.winner(&scores(-50.0, -10.0 - policy.sentence), ""),
            Some(digits)
        );
        // The kanji numerals are compared with the kanji display.
        assert_eq!(first.winner(&scores(-20.0, -11.0), ""), Some(kanji));
    }

    #[test]
    fn a_comparison_spelling_leaves_the_first_alternatives_their_places() {
        use super::{FocusCandidate, Job, Segment, contextual_proposal, spoken};
        let policy = &super::TEST_POLICY;
        let option = |value: &str| FocusCandidate {
            reading: String::new(),
            annotation: String::new(),
            value: value.into(),
            transliteration: false,
            history_deletable: false,
        };
        // Seven segments, each with one alternative. `more` adds a second
        // alternative of the first segment.
        let words = [
            ("よん", "四", "読ん"),
            ("きゅうから", "級から", "球から"),
            ("しょだんまで", "初段まで", "書壇まで"),
            ("ぜんぶ", "全部", "ぜんぶ"),
            ("おなじ", "同じ", "おなじ"),
            ("きじゅんで", "基準で", "規準で"),
            ("はかる", "測る", "計る"),
        ];
        let window = |count: usize, more: bool, after: &str| {
            let parts: Vec<_> = words[..count]
                .iter()
                .map(|(reading, value, _)| Segment {
                    reading: (*reading).into(),
                    value: (*value).into(),
                    ..Default::default()
                })
                .collect();
            let mut lists: Vec<_> = words[..count]
                .iter()
                .map(|(_, value, other)| vec![option(value), option(other)])
                .collect();
            lists[0].extend(more.then(|| option("詠ん")));
            let job = Job {
                context: "検定は".into(),
                reading: "ヨンキュウカラショダンマデゼンブオナジキジュンデハカル".into(),
                prefix: String::new(),
                suffix: String::new(),
                candidates: vec![],
            };
            let numbers = spoken::numbers(&parts, &lists);
            contextual_proposal(&parts, &lists, job, policy, &numbers, "", after)
        };
        let places = |item: &super::Proposal| {
            let index = |text: &str| item.job.candidates.iter().position(|c| c == text);
            let tail = &item.job.candidates[0]["四級から".len()..];
            (
                index(&format!("四九から{tail}")),
                index(&format!("49から{tail}")),
                index(&format!("4級から{tail}")),
            )
        };

        // With room among the nine, 4級から… is scored behind the alternatives.
        let item = window(5, false, "");
        assert_eq!(places(&item), (Some(1), Some(2), Some(8)));
        assert_eq!(item.references, [8]);
        // It takes the place of a second alternative, not of a first one.
        let item = window(5, true, "");
        assert_eq!(places(&item), (Some(1), Some(2), Some(8)));
        assert_eq!(item.job.candidates[7], "四級から初段まで全部おなじ");
        assert_eq!(item.job.candidates[9], "詠ん級から初段まで全部同じ");

        // The displayed text, the number twice and six first alternatives
        // fill the nine places. The kanji numerals give theirs to 4級から….
        let item = window(7, false, "");
        assert_eq!(places(&item), (Some(9), Some(1), Some(8)));
        assert_eq!(item.references, [8]);
        assert_eq!(item.original_count, 9);
        assert_eq!(item.job.candidates[7], "四級から初段まで全部同じ規準で測る");
        let (kanji, digits, shown) = (9, 1, 8);
        let scores = |kanji_score: Option<f32>, display: f32| {
            let mut scores = vec![Some(-60.0); item.job.candidates.len()];
            scores[0] = Some(-30.0);
            scores[kanji] = kanji_score;
            scores[digits] = Some(-10.0);
            scores[shown] = Some(display);
            scores
        };
        // The digits are not weighed against the kanji display.
        assert_eq!(item.winner(&scores(None, -11.0), ""), Some(0));
        // With their lead over 4級から…, the number is read. Its notation is
        // still to be compared: the kanji numerals were not scored.
        assert_eq!(item.winner(&scores(None, -40.0), ""), Some(kanji));
        let notation = item.numeral_choice(kanji, "").unwrap();
        assert_eq!(
            notation.job.candidates,
            [
                "四九から初段まで全部同じ基準で測る",
                "49から初段まで全部同じ基準で測る"
            ]
        );
        // Shortlisted kanji numerals are compared at once.
        assert_eq!(item.winner(&scores(Some(-50.0), -40.0), ""), Some(digits));

        // Digits that the safeguards refuse are not worth the place: a
        // transliterated name follows, and the weights keep 四級 beside it.
        let item = window(7, false, "、アレクサンダー・ミラー、");
        assert_eq!(places(&item), (Some(1), Some(2), None));
        assert!(item.references.is_empty());
    }

    #[test]
    fn digits_that_the_safeguards_refuse_leave_the_kanji_numerals_to_speak() {
        use super::{FocusCandidate, Job, MAX_WINDOWS, Segment, scored_windows};
        // Weights that keep kanji as displayed beside eight katakana characters.
        let policy = &super::TEST_POLICY;
        let option = |value: &str| FocusCandidate {
            reading: String::new(),
            annotation: String::new(),
            value: value.into(),
            transliteration: false,
            history_deletable: false,
        };
        let parts: Vec<_> = [("ろく", "ロク"), ("なな", "七"), ("しーずん", "シーズン")]
            .iter()
            .map(|(reading, value)| Segment {
                reading: (*reading).into(),
                value: (*value).into(),
                ..Default::default()
            })
            .collect();
        let lists = [
            vec![option("ロク"), option("六")],
            vec![option("七")],
            vec![option("シーズン")],
        ];
        let job = Job {
            context: "加入2年目の".into(),
            reading: "ロクナナシーズン".into(),
            prefix: String::new(),
            suffix: String::new(),
            candidates: vec![],
        };
        let mut found =
            scored_windows(&parts, &lists, &job, MAX_WINDOWS, policy, |_, _| Vec::new());
        let index = |text: &str| {
            let candidates = &found[0].job.candidates;
            candidates.iter().position(|c| c == text).unwrap()
        };
        let (kanji, digits) = (index("六七シーズン"), index("67シーズン"));
        let mut scores = vec![Some(-90.0); found[0].job.candidates.len()];
        scores[0] = Some(-60.0);
        scores[kanji] = Some(-50.0);
        scores[digits] = Some(-20.0);
        assert_eq!(found[0].winner(&scores, "2015年の"), Some(digits));
        // A transliterated name follows. With that much katakana the 七 of the
        // display is kept, so the digits cannot replace ロク七. The kanji
        // numerals replace only ロク and still stand for the number; the
        // comparison of notations writes 67 afterwards.
        found[0].after = "、アレクサンダー・ミラー、".into();
        assert_eq!(found[0].winner(&scores, "2015年の"), Some(kanji));
    }

    #[test]
    fn the_numbers_of_a_clause_are_scored_whole_and_carried_into_the_next_look() {
        use super::{FocusCandidate, Job, MAX_WINDOWS, Preview, Segment, scored_windows};
        let policy = &Preview {
            rounds: 2,
            ..super::TEST_POLICY
        };
        let option = |value: &str| FocusCandidate {
            reading: String::new(),
            annotation: String::new(),
            value: value.into(),
            transliteration: false,
            history_deletable: false,
        };
        let segments = |words: &[(&str, &str)]| -> Vec<Segment> {
            words
                .iter()
                .map(|(reading, value)| Segment {
                    reading: (*reading).into(),
                    value: (*value).into(),
                    ..Default::default()
                })
                .collect()
        };
        let job = |reading: &str| Job {
            context: "昨季の".into(),
            reading: reading.into(),
            prefix: String::new(),
            suffix: String::new(),
            candidates: vec![],
        };

        // Eleven segments. The last eight would begin at the last word of 三十三.
        let mut words = vec![
            ("まえは", "前は"),
            ("さん", "三"),
            ("じゅう", "重"),
            ("さん", "三"),
        ];
        words.extend([("かきく", "書き区"); 7]);
        let parts = segments(&words);
        let lists: Vec<_> = parts
            .iter()
            .map(|p| vec![option(&p.value), option("柿苦")])
            .collect();
        let reading = format!("マエハサンジュウサン{}", "カキク".repeat(7));
        let found = scored_windows(
            &parts,
            &lists,
            &job(&reading),
            MAX_WINDOWS,
            policy,
            |_, _| Vec::new(),
        );
        // That window ends up one segment shorter, and the next one has the number.
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].before, "前は三重三");
        assert_eq!(found[0].lattice.as_ref().unwrap().parts.len(), 7);
        assert!(found[0].numbers.is_empty());
        assert_eq!(found[1].after, "書き区".repeat(7));
        assert_eq!(
            found[1].job.candidates[..3],
            ["前は三重三", "前は三十三", "前は33"]
        );

        // Two numbers in one window.
        let parts = segments(&[
            ("せんしゅは", "選手は"),
            ("はち", "八"),
            ("いち", "市"),
            ("しあいで", "試合で"),
            ("に", "二"),
            ("ご", "語"),
            ("てん", "点"),
        ]);
        let lists = [
            vec![option("選手は")],
            vec![option("八"), option("鉢")],
            vec![option("市")],
            vec![option("試合で")],
            vec![option("二")],
            vec![option("語"), option("後")],
            vec![option("点")],
        ];
        let found = scored_windows(
            &parts,
            &lists,
            &job("センシュハハチイチシアイデニゴテン"),
            MAX_WINDOWS,
            policy,
            |_, _| Vec::new(),
        );
        let first = &found[0];
        assert_eq!(
            first.job.candidates[..5],
            [
                "選手は八市試合で二語点",
                "選手は八一試合で二語点",
                "選手は81試合で二語点",
                "選手は八市試合で二五点",
                "選手は八市試合で25点"
            ]
        );
        // With the first number accepted, the second is still offered, now
        // one segment earlier.
        let second = first.next_round(1).unwrap();
        assert_eq!(
            second.job.candidates[..3],
            [
                "選手は八一試合で二語点",
                "選手は八一試合で二五点",
                "選手は八一試合で25点"
            ]
        );
        // A segment that was respelled is no longer the display in which a
        // number was read. The other number stays.
        let respelled = "選手は八市試合で二後点".to_owned();
        let index = first
            .job
            .candidates
            .iter()
            .position(|c| *c == respelled)
            .unwrap();
        let second = first.next_round(index).unwrap();
        assert_eq!(second.numbers.len(), 1);
        assert_eq!(
            second.job.candidates[..3],
            [
                "選手は八市試合で二後点",
                "選手は八一試合で二後点",
                "選手は81試合で二後点"
            ]
        );
    }

    #[test]
    fn a_calendar_year_is_one_fixed_segment_among_the_scored_ones() {
        use super::{FocusCandidate, Segment, calendar, with_years};
        let option = |value: &str| FocusCandidate {
            reading: String::new(),
            annotation: String::new(),
            value: value.into(),
            transliteration: false,
            history_deletable: false,
        };
        let parts: Vec<_> = [
            ("せん", "船"),
            ("きゅうひゃく", "九百"),
            ("きゅうじゅう", "九十"),
            ("さんねん", "三年"),
            ("さんがつ", "３月"),
            ("ばしょに", "場所に"),
        ]
        .iter()
        .map(|(reading, value)| Segment {
            reading: (*reading).into(),
            value: (*value).into(),
            ..Default::default()
        })
        .collect();
        let lists: Vec<_> = parts
            .iter()
            .map(|p| vec![option(&p.value), option("別")])
            .collect();
        let spans = calendar::years(&parts, "").unwrap();
        let (segments, choices) = with_years(&parts, &lists, &spans);
        let values: Vec<_> = segments.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, ["1993年", "３月", "場所に"]);
        assert_eq!(segments[0].reading, "せんきゅうひゃくきゅうじゅうさんねん");
        assert_eq!(choices[0].len(), 1);
        assert_eq!(choices[1].len(), 2);
    }

    #[test]
    fn ending_keeps_exactly_one_terminal_mark() {
        use super::{Ending, Segment};
        let make = |reading: &str, value: &str| Segment {
            reading: reading.to_owned(),
            value: value.to_owned(),
            ..Default::default()
        };
        let base = make("のりものに", "乗り物に");
        for parts in [
            vec![base.clone(), make("のります。", "乗ります。")],
            vec![base.clone(), make("のります", "乗ります"), make("。", "。")],
        ] {
            let span = Ending::parse(&parts).unwrap();
            assert_eq!(
                (span.prefix.len(), span.displayed, span.mark),
                (1, "乗ります", "。")
            );
        }
        assert!(Ending::parse(&[base.clone(), make("のります。", "乗ります．")]).is_none());
        assert!(
            Ending::parse(&[
                base.clone(),
                make("、", "、"),
                make("のります。", "乗ります。")
            ])
            .is_none()
        );
        assert!(Ending::parse(&[base, make("のります", "乗ります"), make("。", "…")]).is_none());
    }

    #[test]
    fn strong_context_scores_preserve_writing_inside_names() {
        for (shown, proposed) in [
            ("アリス・ミラーの作品", "アリスミラーの作品"),
            ("旧西山町に着く", "旧にしやまちょうに着く"),
            ("オレゴン州に移る", "乙州に移る"),
            ("ブルーサンダース東山ブルー", "ブルーサンダース頭ブルー"),
        ] {
            assert!(!preserves_name_spelling(shown, proposed, 8));
        }
        assert!(preserves_name_spelling(
            "検討がつかない",
            "見当がつかない",
            8
        ));
        assert!(preserves_name_spelling(
            "掲載・検討が必要",
            "掲載・見当が必要",
            8
        ));
        assert!(preserves_name_spelling(
            "木の下に経っている",
            "木の下に立っている",
            8
        ));
        // Weights registered without a katakana limit may respell a word beside
        // a long transliterated name. Names themselves stay as displayed.
        let (shown, proposed) = ("ブルーサンダースの講演", "ブルーサンダースの公演");
        assert!(!preserves_name_spelling(shown, proposed, 8));
        assert!(preserves_name_spelling(shown, proposed, 9));
        assert!(preserves_name_spelling(shown, proposed, 0));
        for (shown, proposed) in [
            ("アリス・ミラーの作品", "アリスミラーの作品"),
            ("旧西山町に着く", "旧にしやまちょうに着く"),
            ("オレゴン州に移る", "乙州に移る"),
            ("ブルーサンダース東山ブルー", "ブルーサンダース頭ブルー"),
        ] {
            assert!(!preserves_name_spelling(shown, proposed, 0));
        }
    }
}
