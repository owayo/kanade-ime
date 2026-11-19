// SPDX-License-Identifier: MIT
//! Background reordering of displayed Mozc candidates.
//!
//! The key path only queues a bounded job and later looks its result up. A
//! document-context result reorders unselected choices. Sentence previews
//! for an empty document context are handled in the sentence module.
use super::*;
use kanade_llm::{Job, Poll, Rerank, limits};
use std::time::Duration;

/// Shown next to a candidate the model prefers over the displayed one.
const HINT: &str = "文脈";

pub(super) struct Refinement {
    job: u64,
    revision: Revision,
    candidate_revision: u64,
    /// Scored choices: their slots and exact surfaces at submission.
    slots: Vec<usize>,
    values: Vec<String>,
    submitted: Instant,
}
/// The last delivered update, so a repeated poll gets the same answer.
pub(super) struct Applied {
    revision: Revision,
    previous: u64,
    page: CandidatePage,
}
/// An applied order. It is kept while the same candidates stay on screen, so
/// navigating the list does not shuffle it back.
pub(super) struct Frozen {
    segment: usize,
    reading: String,
    values: Vec<String>,
    hint: Option<String>,
    pub(super) evaluated: bool,
}
impl Frozen {
    pub(super) fn capture(snapshot: &Snapshot, choices: &[candidates::Choice]) -> Option<Self> {
        Some(Self {
            segment: snapshot.focused_segment,
            reading: snapshot
                .preedit
                .segments
                .get(snapshot.focused_segment)?
                .reading
                .clone(),
            values: choices.iter().map(|c| c.value.clone()).collect(),
            hint: None,
            evaluated: false,
        })
    }
    /// Restores the applied order, or reports that the list is a different one.
    pub(super) fn restore(&self, snapshot: &Snapshot, choices: &mut [candidates::Choice]) -> bool {
        let same = snapshot.focused_segment == self.segment
            && snapshot
                .preedit
                .segments
                .get(self.segment)
                .is_some_and(|segment| segment.reading == self.reading)
            && choices.len() == self.values.len()
            && choices.iter().all(|c| self.values.contains(&c.value));
        if !same {
            return false;
        }
        choices.sort_by_key(|c| self.values.iter().position(|v| *v == c.value));
        for choice in choices {
            if self.hint.as_ref() == Some(&choice.value) && choice.annotation.is_empty() {
                choice.annotation = HINT.into();
            }
        }
        true
    }
}

/// Drops a queued or running job and forgets a delivered update.
pub(super) fn cancel(llm: Option<&dyn Rerank>, session: &mut Session) {
    if let Some(sentence) = session.sentence.take() {
        sentence.cancel(llm);
    }
    if let Some(refinement) = session.refinement.take()
        && let Some(llm) = llm
    {
        llm.cancel(refinement.job);
    }
    session.applied = None;
}

impl<C: ConversionEngine> Engine<C> {
    /// Queues a job for the list in `reply`, if every privacy and size gate
    /// passes. Copies a few short strings; never waits.
    pub(super) fn refine(
        &self,
        session: &mut Session,
        reply: &mut KeyReply,
        privacy: Privacy,
        preceding: &str,
    ) {
        let Some(page) = reply.candidates.as_mut() else {
            return;
        };
        if !self.settings.llm
            || !session.snapshot.converting
            || session.selected_choice >= limits::CANDIDATES
        {
            return;
        }
        let preedit = &session.snapshot.preedit;
        let whole = reading(preedit);
        // Password input never reaches this point; private and unknown contexts
        // and excluded applications are not `Normal`.
        if !kanade_llm::eligible(privacy, &whole, session.manual) {
            return;
        }
        let Some(context) = kanade_llm::context_window(preceding) else {
            return;
        };
        let focused = session.snapshot.focused_segment;
        if focused >= preedit.segments.len() {
            return;
        }
        let surface = |segments: &[Segment]| -> String {
            segments.iter().map(|s| s.value.as_str()).collect()
        };
        let (slots, values): (Vec<usize>, Vec<String>) = session
            .choices
            .iter()
            .enumerate()
            .take(limits::CANDIDATES)
            .filter(|(_, c)| {
                c.source == CandidateSource::Mozc
                    && (1..=limits::CANDIDATE_CHARS).contains(&c.value.chars().count())
            })
            .map(|(slot, c)| (slot, c.value.clone()))
            .unzip();
        let job = Job {
            context,
            reading: kanade_llm::katakana(&whole),
            prefix: surface(&preedit.segments[..focused]),
            suffix: surface(&preedit.segments[focused + 1..]),
            candidates: values.clone(),
        };
        if !job.valid() {
            return;
        }
        let Some(job) = self.llm.as_deref().and_then(|llm| llm.submit(job)) else {
            page.refinement_outcome = RefinementOutcome::Unavailable as i32;
            return;
        };
        session.refinement = Some(Refinement {
            job,
            revision: session.revision,
            candidate_revision: session.candidate_revision,
            slots,
            values,
            submitted: Instant::now(),
        });
        page.refinement_pending = true;
    }

    /// Answers a candidate poll. A stale, early or unknown poll is not an
    /// error: it simply reports that there is no update.
    pub(super) fn poll(
        &mut self,
        session: &mut Session,
        poll: &CandidatePoll,
    ) -> Result<Option<Response>, Code> {
        let revision = poll.revision.ok_or(Code::Invalid)?;
        let answer =
            |candidates: Option<CandidatePage>, pending: bool, outcome: RefinementOutcome| {
                Ok(Some(Response {
                    body: Some(response::Body::Candidates(CandidateUpdate {
                        revision: Some(revision),
                        candidates,
                        pending,
                        refinement_outcome: outcome as i32,
                    })),
                }))
            };
        if revision != session.revision {
            return answer(None, false, RefinementOutcome::Unspecified);
        }
        if let Some(applied) = &session.applied
            && applied.revision == revision
            && applied.previous == poll.candidate_revision
            && applied.page.candidate_revision == session.candidate_revision
        {
            return answer(
                Some(applied.page.clone()),
                false,
                RefinementOutcome::Evaluated,
            );
        }
        if poll.candidate_revision != session.candidate_revision {
            return answer(None, false, RefinementOutcome::Unspecified);
        }
        let Some(refinement) = session.refinement.as_ref() else {
            let outcome = session
                .last_reply
                .as_ref()
                .filter(|reply| reply.revision == Some(revision))
                .and_then(|reply| reply.candidates.as_ref())
                .filter(|page| {
                    page.candidate_revision == poll.candidate_revision && !page.refinement_pending
                })
                .map(CandidatePage::refinement_outcome)
                .unwrap_or_default();
            return answer(None, false, outcome);
        };
        let llm = self.llm.as_deref();
        let current = self.settings.llm
            && !session.manual
            && session.snapshot.converting
            && refinement.revision == session.revision
            && refinement.candidate_revision == session.candidate_revision;
        let expired = refinement.submitted.elapsed() > Duration::from_millis(limits::RESULT_TTL_MS);
        let (page, outcome) = if !current || expired {
            cancel(llm, session);
            (
                None,
                if current {
                    RefinementOutcome::TimedOut
                } else {
                    RefinementOutcome::Unspecified
                },
            )
        } else if session.pending.is_some() {
            // The frontend has not acknowledged the event that built this list.
            return answer(None, true, RefinementOutcome::Unspecified);
        } else {
            match llm.map_or(Poll::Gone, |llm| llm.take(refinement.job)) {
                Poll::Pending => return answer(None, true, RefinementOutcome::Unspecified),
                Poll::Gone => {
                    session.refinement = None;
                    (None, RefinementOutcome::Unavailable)
                }
                Poll::Ready(scores) => {
                    let refinement = session.refinement.take().ok_or(Code::Invalid)?;
                    match self.reorder(session, &refinement, &scores) {
                        Ok(page) => (page, RefinementOutcome::Evaluated),
                        Err(()) => (None, RefinementOutcome::Unavailable),
                    }
                }
            }
        };
        if let Some(last) = session.last_reply.as_mut()
            && last.revision == Some(revision)
            && let Some(displayed) = last.candidates.as_mut()
            && displayed.candidate_revision == poll.candidate_revision
        {
            if let Some(updated) = &page {
                *displayed = updated.clone();
            } else {
                displayed.refinement_pending = false;
                displayed.refinement_outcome = outcome as i32;
            }
        }
        answer(page, false, outcome)
    }

    /// Applies scores to the current list. Returns the new page, or `None`
    /// for valid scores that leave the order as it is.
    fn reorder(
        &self,
        session: &mut Session,
        refinement: &Refinement,
        scores: &[Option<f32>],
    ) -> Result<Option<CandidatePage>, ()> {
        if scores.iter().all(Option::is_none)
            || scores.len() != refinement.slots.len()
            || scores.iter().flatten().any(|score| !score.is_finite())
        {
            return Err(());
        }
        // Every scored choice must still be the same Mozc choice in the same slot.
        for (slot, value) in refinement.slots.iter().zip(&refinement.values) {
            let choice = session.choices.get(*slot).ok_or(())?;
            if choice.source != CandidateSource::Mozc || choice.value != *value {
                return Err(());
            }
        }
        let selected = session.selected_choice;
        let mut selected_score = None;
        let mut movable = Vec::with_capacity(scores.len());
        for (slot, score) in refinement.slots.iter().zip(scores) {
            match score {
                Some(score) if *slot == selected => selected_score = Some(*score),
                Some(score) => movable.push((*slot, *score)),
                None => {}
            }
        }
        // The stable sort keeps Mozc's order between equal scores.
        let mut ranked = movable.clone();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        if movable.len() < 2 || ranked.iter().map(|r| r.0).eq(movable.iter().map(|m| m.0)) {
            if session.frozen.is_none() {
                session.frozen = Frozen::capture(&session.snapshot, &session.choices);
            }
            if let Some(order) = &mut session.frozen {
                order.evaluated = true;
            }
            return Ok(None);
        }
        let next_revision = session.candidate_revision.checked_add(1).ok_or(())?;
        let segment = session.snapshot.focused_segment;
        let reading = session
            .snapshot
            .preedit
            .segments
            .get(segment)
            .ok_or(())?
            .reading
            .clone();
        let previous = session.choices.clone();
        for ((target, _), (source, _)) in movable.iter().zip(&ranked) {
            session.choices[*target] = previous[*source].clone();
        }
        let mut hint = None;
        let best = &mut session.choices[movable[0].0];
        if selected_score.is_none_or(|score| ranked[0].1 > score) && best.annotation.is_empty() {
            best.annotation = HINT.into();
            hint = Some(best.value.clone());
        }
        // Old handles name other slots now; a new revision retires them.
        session.candidate_revision = next_revision;
        session.frozen = Some(Frozen {
            segment,
            reading,
            evaluated: true,
            values: session.choices.iter().map(|c| c.value.clone()).collect(),
            hint,
        });
        let page = self.page(session, &session.snapshot).ok_or(())?;
        session.applied = Some(Applied {
            revision: session.revision,
            previous: refinement.candidate_revision,
            page: page.clone(),
        });
        Ok(Some(page))
    }
}
