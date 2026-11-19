// SPDX-License-Identifier: MIT
//! Score an initial conversion against alternatives from a disposable Mozc session.
mod boundary_data;
mod calendar;
mod clause;
mod search;

use super::*;
use kanade_llm::{Job, Poll, Rerank, limits};
use kanade_mozc::FocusCandidate;
use std::time::Duration;

const ENDING_MARGIN: f32 = 1.5;
const SENTENCE_MARGIN: f32 = 5.0;
const RANK_PENALTY: f32 = 0.75;

struct Proposal {
    job: Job,
    before: String,
    after: String,
    penalties: Vec<f32>,
    margin: f32,
    original_count: usize,
}

impl Proposal {
    fn render(&self, index: usize) -> String {
        format!(
            "{}{}{}{}",
            self.before, self.job.prefix, self.job.candidates[index], self.after
        )
    }
}

pub(super) struct Pending {
    started: Instant,
    context: String,
    year: Option<String>,
    work: Option<(u64, Proposal)>,
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
            if context.is_empty() {
                if (punctuation && !is_default) || Ending::parse(parts).is_none() {
                    return None;
                }
            } else {
                let segment_limit = 8;
                if !is_default
                    || !(1..=segment_limit).contains(&parts.len())
                    || (!punctuation
                        && parts.iter().map(|s| s.value.chars().count()).sum::<usize>()
                            > limits::CANDIDATE_CHARS)
                {
                    return None;
                }
            }
        }
        Some(Self {
            started: Instant::now(),
            context,
            year,
            work: None,
        })
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

    fn proposal(&self, last: &[FocusCandidate], mut job: Job) -> Option<Proposal> {
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
            job,
            margin: ENDING_MARGIN,
        })
    }
}

// Mozc can offer a reading in kana for an entire name, or omit the dots in
// a transliterated name. Those are useful manual choices even when a small
// language model strongly prefers the more common spelling.
fn preserves_name_spelling(original: &str, candidate: &str) -> bool {
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
        || (old_kanji > 0 && katakana(&original) >= 8))
}

fn contextual_proposal(parts: &[Segment], lists: &[Vec<FocusCandidate>], mut job: Job) -> Proposal {
    job.candidates
        .push(parts.iter().map(|s| s.value.as_str()).collect());
    let mut penalties = vec![0.0];
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
                    penalties.push(rank as f32 * RANK_PENALTY);
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
        margin: SENTENCE_MARGIN,
        original_count,
    };
    search::widen(&mut p, parts, lists);
    p
}

// Keep the typing session untouched. Check the reconstructed segmentation before using each list.
fn alternatives<C: ConversionEngine>(
    backend: &mut C,
    original: &Snapshot,
    context: &str,
) -> Option<Proposal> {
    let parts = &original.preedit.segments;
    let whole = reading(&original.preedit);
    let ending = if context.is_empty() {
        Some(Ending::parse(parts)?)
    } else {
        None
    };
    let split_deadline = Instant::now() + Duration::from_millis(80);
    let probe = backend.create_session().ok()?;
    let collect = |backend: &mut C| -> Option<Proposal> {
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
                    return ending.proposal(&snap.candidates, job);
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
        if whole.contains(['、', '。']) {
            clause::proposal(parts, &lists, &job, |segments, p| {
                search::resegment(backend, probe, segments, p, split_deadline)
            })
        } else {
            let mut proposal = contextual_proposal(parts, &lists, job);
            search::resegment(backend, probe, parts, &mut proposal, split_deadline);
            proposal.job.valid_for(true).then_some(proposal)
        }
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
        if let Some(value) = pending.year.take() {
            if !self.settings.calculated_dates {
                return status(false, RefinementOutcome::Unspecified);
            }
            return self.apply_sentence(session, seq, poll, value, CandidateSource::Date);
        }
        if pending.work.is_none() {
            let Some(proposal) =
                alternatives(&mut self.backend, &session.snapshot, &pending.context)
            else {
                return status(false, RefinementOutcome::Unspecified);
            };
            let ticket = if pending.context.is_empty() {
                llm.submit(proposal.job.clone())
            } else {
                llm.submit_sentence(proposal.job.clone())
            };
            let Some(ticket) = ticket else {
                return status(false, RefinementOutcome::Unavailable);
            };
            pending.work = Some((ticket, proposal));
        }
        let (ticket, proposal) = pending.work.as_ref().ok_or(Code::Invalid)?;
        let job = &proposal.job;
        let scores = match llm.take(*ticket) {
            Poll::Pending => {
                session.sentence = Some(pending);
                return status(true, RefinementOutcome::Unspecified);
            }
            Poll::Gone => return status(false, RefinementOutcome::Unavailable),
            Poll::Ready(scores) => scores,
        };
        if scores.len() != job.candidates.len()
            || scores[..proposal.original_count]
                .iter()
                .any(Option::is_none)
            || scores.iter().flatten().any(|s| !s.is_finite())
        {
            return status(false, RefinementOutcome::Unavailable);
        }
        // Only change the provisional spelling with a clear advantage over
        // both Mozc's default and the other spellings under consideration.
        let scores: Vec<_> = scores
            .iter()
            .zip(&proposal.penalties)
            .map(|(s, p)| s.map_or(f32::NEG_INFINITY, |v| v - p))
            .collect();
        let mut order: Vec<_> = (0..scores.len()).collect();
        order.sort_by(|a, b| scores[*b].total_cmp(&scores[*a]));
        let originals: Vec<_> = order
            .iter()
            .copied()
            .filter(|&i| i < proposal.original_count)
            .collect();
        let mut winner = 0;
        if originals.len() >= 2 && scores[originals[0]] - scores[originals[1]] >= proposal.margin {
            winner = originals[0];
        }
        let best = order[0];
        if best >= proposal.original_count
            && scores[best] - scores[order[1]] >= proposal.margin + 2.0
        {
            winner = best;
        }
        if winner == 0
            || (!job.context.is_empty()
                && !preserves_name_spelling(&proposal.render(0), &proposal.render(winner)))
        {
            return status(false, RefinementOutcome::Evaluated);
        }
        self.apply_sentence(
            session,
            seq,
            poll,
            proposal.render(winner),
            CandidateSource::Llm,
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
            assert!(!preserves_name_spelling(shown, proposed));
        }
        assert!(preserves_name_spelling("検討がつかない", "見当がつかない"));
        assert!(preserves_name_spelling(
            "掲載・検討が必要",
            "掲載・見当が必要"
        ));
        assert!(preserves_name_spelling(
            "木の下に経っている",
            "木の下に立っている"
        ));
    }
}
