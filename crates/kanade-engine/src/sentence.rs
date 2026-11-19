// SPDX-License-Identifier: MIT
//! Score an initial conversion against alternatives from a disposable Mozc session.
mod calendar;
mod clause;

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
                if punctuation
                    || !(2..=4).contains(&parts.len())
                    || ending(&parts.last()?.value).is_none()
                {
                    return None;
                }
            } else {
                let segment_limit = if punctuation { 8 } else { 5 };
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

fn ending_proposal(parts: &[Segment], last: &[FocusCandidate], mut job: Job) -> Option<Proposal> {
    let displayed = &parts.last()?.value;
    let suffix = ending(displayed)?;
    job.prefix = parts[..parts.len() - 1]
        .iter()
        .map(|s| s.value.as_str())
        .collect();
    job.candidates.push(displayed.clone());
    for c in last.iter().take(limits::CANDIDATES) {
        if !c.transliteration
            && ending(&c.value) == Some(suffix)
            && !job.candidates.contains(&c.value)
            && job.candidates.len() < limits::CANDIDATES
        {
            job.candidates.push(c.value.clone());
        }
    }
    job.valid().then(|| Proposal {
        before: String::new(),
        after: String::new(),
        penalties: vec![0.; job.candidates.len()],
        job,
        margin: ENDING_MARGIN,
    })
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

fn contextual_proposal(
    parts: &[Segment],
    lists: &[Vec<FocusCandidate>],
    mut job: Job,
) -> Option<Proposal> {
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
    job.valid().then_some(Proposal {
        job,
        before: String::new(),
        after: String::new(),
        penalties,
        margin: SENTENCE_MARGIN,
    })
}

// Keep the typing session untouched. Check the reconstructed segmentation before using each list.
fn alternatives<C: ConversionEngine>(
    backend: &mut C,
    original: &Snapshot,
    context: &str,
) -> Option<Proposal> {
    let parts = &original.preedit.segments;
    let whole = reading(&original.preedit);
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
            if context.is_empty() {
                if pos + 1 == parts.len() {
                    return ending_proposal(parts, &snap.candidates, job);
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
                    if list.len() == 4 {
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
            clause::proposal(parts, &lists, &job)
        } else {
            contextual_proposal(parts, &lists, job)
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
            let Some(ticket) = llm.submit(proposal.job.clone()) else {
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
        let Some(mut scores) = scores.into_iter().collect::<Option<Vec<_>>>() else {
            return status(false, RefinementOutcome::Unavailable);
        };
        if scores.len() != job.candidates.len() || scores.iter().any(|s| !s.is_finite()) {
            return status(false, RefinementOutcome::Unavailable);
        }
        // Only change the provisional spelling with a clear advantage over
        // both Mozc's default and the other spellings under consideration.
        for (score, penalty) in scores.iter_mut().zip(&proposal.penalties) {
            *score -= penalty;
        }
        let mut order: Vec<_> = (0..scores.len()).collect();
        order.sort_by(|a, b| scores[*b].total_cmp(&scores[*a]));
        let winner = order[0];
        if winner == 0
            || scores[winner] - scores[order[1]] < proposal.margin
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
