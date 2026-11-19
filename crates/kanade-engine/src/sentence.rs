//! Score the last inflected segment of a short conversion using its own prefix.
use super::*;
use kanade_llm::{Job, Poll, Rerank, limits};
use std::time::Duration;

const MIN_SCORE_MARGIN: f32 = 1.5;

pub(super) struct Pending {
    started: Instant,
    work: Option<(u64, Job)>,
}

impl Pending {
    pub(super) fn new() -> Self {
        Self {
            started: Instant::now(),
            work: None,
        }
    }

    pub(super) fn cancel(self, llm: Option<&dyn Rerank>) {
        if let (Some((ticket, _)), Some(llm)) = (self.work, llm) {
            llm.cancel(ticket);
        }
    }
}

// An alternative spelling of an inflected word retains its kana ending.
// The training contrasts cover single-kanji verbs. Leave compound stems and
// pure kana to the ordinary converter.
fn ending(surface: &str) -> Option<&str> {
    let split = surface
        .trim_end_matches(|c| ('ぁ'..='ゖ').contains(&c))
        .len();
    let (stem, kana) = surface.split_at(split);
    let mut chars = stem.chars();
    let single_kanji =
        chars.next().is_some_and(|c| ('一'..='鿿').contains(&c)) && chars.next().is_none();
    (single_kanji && kana.chars().count() >= 2).then_some(kana)
}

pub(super) fn eligible(snapshot: &Snapshot) -> bool {
    (2..=4).contains(&snapshot.preedit.segments.len())
        && snapshot
            .preedit
            .segments
            .last()
            .is_some_and(|s| ending(&s.value).is_some())
}

fn key(special: SpecialKey) -> Key {
    Key {
        special: special as i32,
        ..Default::default()
    }
}

// A disposable Mozc session supplies its candidates without changing the
// selected segment, conversion state or history of the typing session.
fn alternatives<C: ConversionEngine>(backend: &mut C, original: &Snapshot) -> Option<Job> {
    let parts = &original.preedit.segments;
    let last = parts.last()?;
    let expected_ending = ending(&last.value)?;
    let whole = reading(&original.preedit);
    let probe = backend.create_session().ok()?;
    let collect = |backend: &mut C| -> Option<Job> {
        backend.restore(probe, &whole).ok()?;
        let mut snap = backend.key(probe, &key(SpecialKey::Space), "").ok()?;
        for position in 0..parts.len() {
            if !snap.converting
                || snap.commit.is_some()
                || snap.focused_segment != position
                || !snap
                    .preedit
                    .segments
                    .iter()
                    .map(|s| &s.reading)
                    .eq(parts.iter().map(|s| &s.reading))
            {
                return None;
            }
            if position + 1 < parts.len() {
                snap = backend.key(probe, &key(SpecialKey::Right), "").ok()?;
            }
        }
        let mut candidates = vec![last.value.clone()];
        for candidate in snap.candidates.iter().take(limits::CANDIDATES) {
            if !candidate.transliteration
                && ending(&candidate.value) == Some(expected_ending)
                && !candidates.contains(&candidate.value)
                && candidates.len() < limits::CANDIDATES
            {
                candidates.push(candidate.value.clone());
            }
        }
        let job = Job {
            context: String::new(),
            reading: kanade_llm::katakana(&whole),
            prefix: parts[..parts.len() - 1]
                .iter()
                .map(|s| s.value.as_str())
                .collect(),
            suffix: String::new(),
            candidates,
        };
        job.valid().then_some(job)
    };
    let job = collect(backend);
    backend.delete_session(probe).ok()?;
    job
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
            || !session.preceding.is_empty()
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
            let Some(job) = alternatives(&mut self.backend, &session.snapshot) else {
                return status(false, RefinementOutcome::Unspecified);
            };
            let Some(ticket) = llm.submit(job.clone()) else {
                return status(false, RefinementOutcome::Unavailable);
            };
            pending.work = Some((ticket, job));
        }
        let (ticket, job) = pending.work.as_ref().ok_or(Code::Invalid)?;
        let scores = match llm.take(*ticket) {
            Poll::Pending => {
                session.sentence = Some(pending);
                return status(true, RefinementOutcome::Unspecified);
            }
            Poll::Gone => return status(false, RefinementOutcome::Unavailable),
            Poll::Ready(scores) => scores,
        };
        let Some(scores) = scores.into_iter().collect::<Option<Vec<_>>>() else {
            return status(false, RefinementOutcome::Unavailable);
        };
        if scores.len() != job.candidates.len() || scores.iter().any(|s| !s.is_finite()) {
            return status(false, RefinementOutcome::Unavailable);
        }
        // Only change the provisional spelling with a clear advantage over
        // both Mozc's default and the other spellings under consideration.
        let mut order: Vec<_> = (0..scores.len()).collect();
        order.sort_by(|a, b| scores[*b].total_cmp(&scores[*a]));
        let winner = order[0];
        if winner == 0 || scores[winner] - scores[order[1]] < MIN_SCORE_MARGIN {
            return status(false, RefinementOutcome::Evaluated);
        }
        let value = format!("{}{}", job.prefix, job.candidates[winner]);
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
