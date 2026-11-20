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

/// A segment whose displayed spelling a candidate replaces.
type Change = (usize, String);

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
    /// The scored segments and their dictionary candidates.
    lattice: Option<(Vec<Segment>, Vec<Vec<FocusCandidate>>)>,
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

    // A text with one confirmed correction often needs another. Look once
    // more around the accepted spelling, with the same context and reading.
    fn next_round(&self, winner: usize) -> Option<Proposal> {
        if self.numerals || self.round >= self.policy.rounds || winner == 0 {
            return None;
        }
        let (parts, lists) = self.lattice.as_ref()?;
        let mut parts = parts.clone();
        let mut lists = lists.clone();
        for (pos, value) in self.changes.get(winner)?.as_ref()? {
            parts[*pos].value.clone_from(value);
            let list = &mut lists[*pos];
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
        let job = Job {
            candidates: Vec::new(),
            ..self.job.clone()
        };
        let mut item = contextual_proposal(&parts, &lists, job, &self.policy);
        search::paired(&mut item, &parts, &lists);
        item.before.clone_from(&self.before);
        item.after.clone_from(&self.after);
        item.margin = self.margin;
        item.round = self.round + 1;
        item.lattice = Some((parts, lists));
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
        for variant in numerals::variants(text, &self.after) {
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
        let seen = committed.bytes().any(|b| b.is_ascii_digit());
        (count > 1 && job.valid_for(true)).then(|| Proposal {
            job,
            before: self.before.clone(),
            after: self.after.clone(),
            penalties: vec![0.0; count],
            margin: if seen {
                self.policy.digits_seen
            } else {
                self.policy.digits
            },
            original_count: count,
            numerals: true,
            changes: vec![None; count],
            lattice: None,
            round: self.round,
            policy: self.policy,
        })
    }

    // Only change the provisional spelling with a clear advantage over
    // both Mozc's default and the other spellings under consideration.
    fn winner(&self, scores: &[Option<f32>]) -> Option<usize> {
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
        let scores: Vec<_> = scores
            .iter()
            .zip(&self.penalties)
            .map(|(s, p)| s.map_or(f32::NEG_INFINITY, |v| v - p))
            .collect();
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
        if winner != 0
            && !job.context.is_empty()
            && !preserves_name_spelling(&self.render(0), &self.render(winner), self.policy.katakana)
        {
            winner = 0;
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
    let prefix = original
        .iter()
        .zip(&candidate)
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
    let old = &left[..left.len() - suffix];
    let new = &right[..right.len() - suffix];
    let kanji = |text: &[char]| text.iter().filter(|&&c| ('一'..='鿿').contains(&c)).count();
    let katakana = |text: &[char]| {
        text.iter()
            .filter(|&&c| matches!(c, 'ァ'..='ヺ' | 'ー'))
            .count()
    };
    let old_kanji = kanji(old);
    let old_katakana = katakana(old);
    !(old.contains(&'・')
        || new.contains(&'・')
        || (old_kanji >= 2 && kanji(new) < old_kanji)
        || (old_katakana >= 3 && katakana(new) < old_katakana)
        || (old_kanji > 0
            && katakana_limit > 0
            && katakana(&original) >= usize::from(katakana_limit)))
}

fn contextual_proposal(
    parts: &[Segment],
    lists: &[Vec<FocusCandidate>],
    mut job: Job,
    policy: &Preview,
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
    // Take the next distinct alternative at each position before exploring lower ranks.
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
    let original_count = job.candidates.len();
    let mut p = Proposal {
        job,
        before: String::new(),
        after: String::new(),
        penalties,
        margin: policy.sentence,
        original_count,
        numerals: false,
        changes,
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
        let mut item = contextual_proposal(window, choices, job, policy);
        let splits = explore
            .take()
            .map_or_else(Vec::new, |probe| probe(window, &mut item));
        search::paired(&mut item, window, choices);
        search::extend_splits(&mut item, window, choices, &splits);
        item.before = before;
        item.after = parts[end..].iter().map(|s| s.value.as_str()).collect();
        item.lattice = Some((window.to_vec(), choices.to_vec()));
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
                if snap.candidates.first()?.value != parts[pos].value {
                    return None;
                }
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
                    let Some(winner) = proposal.winner(&scores) else {
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
            return Scan::Finished(None, RefinementOutcome::Unavailable);
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
        let policy = &kanade_llm::profile::CATALOG[0].preview;
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
            lattice: None,
            round: 1,
            policy: kanade_llm::profile::CATALOG[0].preview,
        };
        // A lead of five over the display and 3.75 over the runner-up, after penalties.
        assert_eq!(
            proposal.winner(&[Some(-20.0), Some(-14.0), Some(-30.0)]),
            Some(1)
        );
        assert_eq!(
            proposal.winner(&[Some(-20.0), Some(-14.5), Some(-30.0)]),
            Some(0)
        );
        assert_eq!(
            proposal.winner(&[Some(-20.0), Some(-14.0), Some(-16.0)]),
            Some(0)
        );
        assert_eq!(proposal.winner(&[Some(-20.0), None, Some(-30.0)]), None);
        assert_eq!(proposal.winner(&[Some(-20.0), Some(-14.0)]), None);
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
        let policy = kanade_llm::profile::CATALOG[0].preview;
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
        assert_eq!(choice.winner(&[Some(-8.0), Some(-8.0)]), Some(1));
        assert_eq!(choice.winner(&[Some(-8.0), Some(-8.5)]), Some(0));
        // Without them the digits need a clear lead.
        let plain = words.numeral_choice(0, "前の文。").unwrap();
        assert_eq!(plain.margin, policy.digits);
        assert_eq!(
            plain.winner(&[Some(-8.0), Some(-8.0 + policy.digits - 0.5)]),
            Some(0)
        );
        assert_eq!(
            plain.winner(&[Some(-8.0), Some(-8.0 + policy.digits)]),
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
            ..kanade_llm::profile::CATALOG[0].preview
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
