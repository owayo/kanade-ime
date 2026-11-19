// SPDX-License-Identifier: MIT
//! Score an initial conversion against alternatives from a disposable Mozc session.
use super::*;
use kanade_llm::{Job, Poll, Rerank, limits};
use kanade_mozc::FocusCandidate;
use std::time::Duration;

const ENDING_MARGIN: f32 = 1.5;
const SENTENCE_MARGIN: f32 = 5.0;
const RANK_PENALTY: f32 = 0.75;

struct Proposal {
    job: Job,
    penalties: Vec<f32>,
    margin: f32,
}

pub(super) struct Pending {
    started: Instant,
    context: String,
    work: Option<(u64, Proposal)>,
}

impl Pending {
    pub(super) fn prepare(session: &Session, preceding: &str) -> Option<Self> {
        let parts = &session.snapshot.preedit.segments;
        let context = if preceding.is_empty() {
            if !(2..=4).contains(&parts.len()) || ending(&parts.last()?.value).is_none() {
                return None;
            }
            String::new()
        } else {
            // Respect a preference already applied by the installed learning service.
            if session.automatic
                || !(1..=5).contains(&parts.len())
                || parts.iter().map(|s| s.value.chars().count()).sum::<usize>()
                    > limits::CANDIDATE_CHARS
                || session.choices.get(session.selected_choice)?.source != CandidateSource::Mozc
            {
                return None;
            }
            kanade_llm::context_window(preceding)?
        };
        Some(Self {
            started: Instant::now(),
            context,
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
        penalties: vec![0.; job.candidates.len()],
        job,
        margin: ENDING_MARGIN,
    })
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
        contextual_proposal(parts, &lists, job)
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
        if winner == 0 || scores[winner] - scores[order[1]] < proposal.margin {
            return status(false, RefinementOutcome::Evaluated);
        }
        let value = format!("{}{}", job.prefix, job.candidates[winner]);
        refine::cancel(self.llm.as_deref(), session);
        let choice = candidates::Choice::model_sentence(value, reading(&session.snapshot.preedit));
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
            page.refinement_outcome = RefinementOutcome::Evaluated as i32;
        }
        session.last_request = Some(Request {
            body: Some(request::Body::Poll(*poll)),
        });
        session.last_reply = Some(reply.clone());
        Ok(Some(key_response(reply)))
    }
}
