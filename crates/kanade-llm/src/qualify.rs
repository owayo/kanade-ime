// SPDX-License-Identifier: MIT
//! Qualification: measures the actual worker and model against the corpora
//! and gates of the model's registered profile.
//!
//! Nothing here prints or stores a corpus text. A report carries counts, digests
//! and durations only.
use crate::{
    Job,
    frame::Fault,
    katakana, limits,
    process::{Failure, Worker},
    profile::{Model, Profile},
    store::Reason,
};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
struct ParityFile {
    cases: Vec<ParityCase>,
}
#[derive(Deserialize)]
struct ParityCase {
    text: String,
    tokens: Vec<u32>,
}
#[derive(Deserialize)]
struct RankingCase {
    context: String,
    reading: String,
    prefix: String,
    suffix: String,
    candidates: Vec<String>,
    expected: String,
}
fn parity_cases(profile: &Profile) -> Vec<ParityCase> {
    serde_json::from_str::<ParityFile>(profile.parity)
        .map(|file| file.cases)
        .unwrap_or_default()
}
fn ranking_cases(profile: &Profile) -> Vec<(Job, usize)> {
    profile
        .ranking
        .lines()
        .filter_map(|line| serde_json::from_str::<RankingCase>(line).ok())
        .filter_map(|case| {
            let expected = case.candidates.iter().position(|c| *c == case.expected)?;
            Some((
                Job {
                    context: case.context,
                    reading: katakana(&case.reading),
                    prefix: case.prefix,
                    suffix: case.suffix,
                    candidates: case.candidates,
                },
                expected,
            ))
        })
        .collect()
}
pub fn parity_count(profile: &Profile) -> u32 {
    parity_cases(profile).len() as u32
}
pub fn ranking_count(profile: &Profile) -> u32 {
    profile.ranking.lines().count() as u32
}

/// Counts and durations from one run. Readiness is derived from these numbers
/// each time; there is no stored pass flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measurement {
    pub tokenizer_cases: u32,
    pub tokenizer_mismatches: u32,
    pub ranking_cases: u32,
    pub ranking_top1: u32,
    /// Fixture candidates the tokenizer could not represent.
    pub ranking_unscored: u32,
    /// Requests whose repeated scores differed.
    pub unstable: u32,
    /// Over-long, zero-budget and reserved-marker requests that were not
    /// refused as required.
    pub bound_failures: u32,
    pub p50_ms: u32,
    pub p95_ms: u32,
    pub max_ms: u32,
}
impl Measurement {
    /// Judges the counts against the corpora and gates of `profile`. Counts
    /// measured for another profile do not pass.
    pub fn verdict(&self, profile: &Profile) -> Reason {
        if self.tokenizer_cases != parity_count(profile)
            || self.tokenizer_cases == 0
            || self.tokenizer_mismatches != 0
        {
            return Reason::TokenizerMismatch;
        }
        if self.ranking_cases != ranking_count(profile)
            || self.ranking_cases == 0
            || self.ranking_top1 > self.ranking_cases
            || self.ranking_top1 as u64 * 1000
                < self.ranking_cases as u64 * profile.gates.min_ranking_per_mille as u64
            || self.ranking_unscored != 0
            || self.unstable != 0
            || self.bound_failures != 0
            || self.p95_ms > profile.gates.max_p95_ms
            || self.p50_ms > self.p95_ms
            || self.p95_ms > self.max_ms
        {
            return Reason::QualityGate;
        }
        Reason::None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    StartingRuntime,
    Tokenizer,
    Bounds,
    Ranking,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Worker(Failure),
    /// The loaded file, vocabulary or profile is not the registered one.
    Identity,
    Corpus,
}

/// `text` repeated to at most `chars` characters and the frame's byte bound.
fn filler(text: &str, chars: usize) -> String {
    let count = (chars / text.chars().count().max(1)).min(limits::TEXT_BYTES / text.len().max(1));
    text.repeat(count)
}

/// Runs every check of the model's profile against a fresh worker. Takes a
/// few seconds; call it from a management thread only.
pub fn run(
    worker: &Path,
    model_path: &Path,
    model: &'static Model,
    mut progress: impl FnMut(Phase),
) -> Result<Measurement, Refusal> {
    let profile = model.profile;
    let parity = parity_cases(profile);
    let ranking = ranking_cases(profile);
    if !profile.well_formed()
        || parity.is_empty()
        || ranking.is_empty()
        || ranking.len() as u32 != ranking_count(profile)
        || !ranking
            .iter()
            .all(|(job, _)| job.valid() && profile.prompt.admits(job))
    {
        return Err(Refusal::Corpus);
    }
    progress(Phase::StartingRuntime);
    let mut worker = Worker::spawn(worker).map_err(Refusal::Worker)?;
    worker
        .load(model_path, model, limits::THREADS)
        .map_err(|failure| match failure {
            Failure::Identity | Failure::Refused(Fault::ProfileMismatch | Fault::UnknownModel) => {
                Refusal::Identity
            }
            failure => Refusal::Worker(failure),
        })?;
    let timeout = Duration::from_secs(5);

    progress(Phase::Tokenizer);
    let mut tokenizer_mismatches = 0;
    for chunk in parity.chunks(64) {
        let texts: Vec<String> = chunk.iter().map(|case| case.text.clone()).collect();
        let actual = worker.tokenize(&texts, timeout).map_err(Refusal::Worker)?;
        for (case, actual) in chunk.iter().zip(actual) {
            // The reference marks unrepresentable input with the profile's
            // unknown token. The worker must refuse exactly those texts and
            // reproduce all the others.
            let unknown = profile
                .tokenizer
                .unknown
                .is_some_and(|unknown| case.tokens.contains(&unknown));
            let expected = (!unknown).then_some(&case.tokens);
            if actual.as_ref() != expected {
                tokenizer_mismatches += 1;
            }
        }
    }

    progress(Phase::Bounds);
    let mut bound_failures = 0;
    let sample = &ranking[0].0;
    // A request far beyond the token limit must be refused, not truncated.
    let oversized = Job {
        context: filler(profile.oversize, limits::CONTEXT_CHARS),
        reading: "ア".repeat(limits::READING_CHARS),
        prefix: filler(profile.oversize, limits::SURROUND_CHARS),
        ..sample.clone()
    };
    let started = Instant::now();
    match worker.score(&oversized, limits::BUDGET_MS, timeout) {
        Err(Failure::Refused(Fault::TooLong)) => {}
        Ok(scores) if scores.iter().all(Option::is_none) => {}
        Err(failure) if !failure.recoverable() => return Err(Refusal::Worker(failure)),
        _ => bound_failures += 1,
    }
    // With no time budget the decode must stop instead of running to the end.
    match worker.score(sample, 0, timeout) {
        Err(Failure::Refused(Fault::Deadline)) => {}
        Err(failure) if !failure.recoverable() => return Err(Refusal::Worker(failure)),
        _ => bound_failures += 1,
    }
    if started.elapsed() > Duration::from_millis(limits::HARD_TIMEOUT_MS) {
        bound_failures += 1;
    }
    // A marker of this profile in request text must be refused by the worker
    // process itself, not only by the checks in front of it.
    let mut marked = sample.clone();
    marked.candidates[0] = profile.prompt.input.into();
    match worker.score_unchecked(&marked, limits::BUDGET_MS, timeout, false) {
        Err(Failure::Refused(Fault::Reserved)) => {}
        Err(failure) if !failure.recoverable() => return Err(Refusal::Worker(failure)),
        _ => bound_failures += 1,
    }

    progress(Phase::Ranking);
    let (mut top1, mut unscored, mut unstable) = (0, 0, 0);
    let mut times = Vec::with_capacity(ranking.len() * 2);
    let hard = Duration::from_millis(limits::HARD_TIMEOUT_MS);
    for (job, expected) in &ranking {
        let started = Instant::now();
        let first = worker
            .score(job, limits::BUDGET_MS, hard)
            .map_err(Refusal::Worker)?;
        times.push(started.elapsed());
        let started = Instant::now();
        let second = worker
            .score(job, limits::BUDGET_MS, hard)
            .map_err(Refusal::Worker)?;
        times.push(started.elapsed());
        if first != second {
            unstable += 1;
        }
        unscored += first.iter().filter(|score| score.is_none()).count() as u32;
        if best(&first) == Some(*expected) {
            top1 += 1;
        }
    }
    times.sort();
    let ms = |index: usize| times[index.min(times.len() - 1)].as_millis().max(1) as u32;
    Ok(Measurement {
        tokenizer_cases: parity.len() as u32,
        tokenizer_mismatches,
        ranking_cases: ranking.len() as u32,
        ranking_top1: top1,
        ranking_unscored: unscored,
        unstable,
        bound_failures,
        p50_ms: ms(times.len() / 2),
        p95_ms: ms(times.len() * 95 / 100),
        max_ms: ms(times.len() - 1),
    })
}

/// Index of the highest score; ties keep the earlier candidate.
pub fn best(scores: &[Option<f32>]) -> Option<usize> {
    let mut best: Option<(usize, f32)> = None;
    for (index, score) in scores.iter().enumerate() {
        if let Some(score) = score
            && best.is_none_or(|(_, top)| *score > top)
        {
            best = Some((index, *score));
        }
    }
    best.map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{self, fixture};
    fn tsumugi() -> &'static Profile {
        profile::CATALOG[0].profile
    }
    fn passing(profile: &Profile) -> Measurement {
        Measurement {
            tokenizer_cases: parity_count(profile),
            tokenizer_mismatches: 0,
            ranking_cases: ranking_count(profile),
            ranking_top1: ranking_count(profile),
            ranking_unscored: 0,
            unstable: 0,
            bound_failures: 0,
            p50_ms: 20,
            p95_ms: 40,
            max_ms: 60,
        }
    }
    #[test]
    fn the_existing_corpora_are_unchanged_and_within_request_bounds() {
        let ranking = ranking_cases(tsumugi());
        assert_eq!(ranking.len() as u32, ranking_count(tsumugi()));
        assert_eq!((ranking.len(), parity_count(tsumugi())), (85, 373));
        assert!(ranking.iter().all(|(job, _)| job.valid()));
        let parity = parity_cases(tsumugi());
        assert!(parity.iter().any(|case| case.tokens.contains(&257)));
        assert!(parity.iter().all(|case| case.text.chars().count() <= 128));
        assert_eq!(
            tsumugi().parity_sha256(),
            "44bcc1cb0c0b606b159be7d3cc234050346c635cd58f4e9782ebf87139b51555"
        );
        assert_eq!(
            tsumugi().ranking_sha256(),
            "939bf0be275f3dffe27f910f5f64d3348ce2695bde5d321c303628e874c0ad53"
        );
    }
    #[test]
    fn every_registered_profile_has_usable_corpora_of_its_own() {
        for model in profile::models() {
            let profile = model.profile;
            let ranking = ranking_cases(profile);
            assert!(!ranking.is_empty(), "{}", profile.id);
            assert_eq!(ranking.len() as u32, ranking_count(profile));
            // A fixture never contains a marker of its own profile.
            assert!(
                ranking
                    .iter()
                    .all(|(job, _)| job.valid() && profile.prompt.admits(job))
            );
            assert!(parity_count(profile) > 0);
            let oversized = filler(profile.oversize, limits::SURROUND_CHARS);
            assert!(!oversized.is_empty() && oversized.len() <= limits::TEXT_BYTES);
            assert!(oversized.chars().count() <= limits::SURROUND_CHARS);
        }
        assert_ne!(parity_count(tsumugi()), parity_count(&fixture::ALT_PROFILE));
        assert_ne!(
            ranking_count(tsumugi()),
            ranking_count(&fixture::ALT_PROFILE)
        );
    }
    #[test]
    fn verdict_is_derived_from_counts() {
        let tsumugi = tsumugi();
        assert_eq!(passing(tsumugi).verdict(tsumugi), Reason::None);
        type Edit = fn(&mut Measurement);
        let edits: [(Edit, Reason); 10] = [
            (|m| m.tokenizer_mismatches = 1, Reason::TokenizerMismatch),
            (|m| m.tokenizer_cases -= 1, Reason::TokenizerMismatch),
            (|m| m.ranking_cases -= 1, Reason::QualityGate),
            (
                |m| m.ranking_top1 = m.ranking_cases * 89 / 100,
                Reason::QualityGate,
            ),
            (|m| m.ranking_top1 += 1, Reason::QualityGate),
            (|m| m.ranking_unscored = 1, Reason::QualityGate),
            (|m| m.unstable = 1, Reason::QualityGate),
            (|m| m.bound_failures = 1, Reason::QualityGate),
            (
                |m| {
                    m.p95_ms = 301;
                    m.max_ms = 301;
                },
                Reason::QualityGate,
            ),
            (|m| m.p50_ms = m.p95_ms + 1, Reason::QualityGate),
        ];
        for (edit, reason) in edits {
            let mut measurement = passing(tsumugi);
            edit(&mut measurement);
            assert_eq!(measurement.verdict(tsumugi), reason);
        }
        // The measured result of the pinned model still passes its gates.
        let mut measured = passing(tsumugi);
        measured.ranking_top1 = 81;
        assert_eq!(measured.verdict(tsumugi), Reason::None);
        measured.ranking_top1 = 76;
        assert_eq!(measured.verdict(tsumugi), Reason::QualityGate);
    }
    #[test]
    fn counts_are_judged_by_the_gates_and_corpora_of_their_own_profile() {
        let tsumugi = tsumugi();
        let alt = &fixture::ALT_PROFILE;
        let other = &fixture::OTHER_PROFILE;
        // Counts of one profile never satisfy another.
        assert_eq!(passing(alt).verdict(alt), Reason::None);
        assert_eq!(passing(alt).verdict(tsumugi), Reason::TokenizerMismatch);
        assert_eq!(passing(tsumugi).verdict(alt), Reason::TokenizerMismatch);
        // No correct choice at all passes only where the gate is zero.
        let mut none = passing(alt);
        none.ranking_top1 = 0;
        assert_eq!(none.verdict(alt), Reason::None);
        let mut none = passing(tsumugi);
        none.ranking_top1 = 0;
        assert_eq!(none.verdict(tsumugi), Reason::QualityGate);
        // The time gate is the profile's own.
        let mut slow = passing(alt);
        (slow.p95_ms, slow.max_ms) = (350, 350);
        assert_eq!(slow.verdict(alt), Reason::None);
        let mut slow = passing(tsumugi);
        (slow.p95_ms, slow.max_ms) = (350, 350);
        assert_eq!(slow.verdict(tsumugi), Reason::QualityGate);
        let mut slow = passing(other);
        (slow.p95_ms, slow.max_ms) = (51, 60);
        assert_eq!(slow.verdict(other), Reason::QualityGate);
        (slow.p95_ms, slow.max_ms) = (40, 60);
        assert_eq!(slow.verdict(other), Reason::None);
    }
    #[test]
    fn best_ignores_unscored_candidates() {
        assert_eq!(best(&[None, Some(-3.0), Some(-1.0), Some(-1.0)]), Some(2));
        assert_eq!(best(&[None, None]), None);
    }
}
