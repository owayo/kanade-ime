// SPDX-License-Identifier: MIT
//! Propose new spellings without changing the live document or its Mozc session.
use super::*;

const SEGMENT_DEPTH: usize = 32;
const SPLIT_RESERVE: usize = 18;

fn substitute(parts: &[Segment], pos: usize, word: &str) -> String {
    parts
        .iter()
        .enumerate()
        .map(|(i, p)| if i == pos { word } else { &p.value })
        .collect()
}

impl Proposal {
    fn offer(&mut self, value: String, penalty: f32, limit: usize) {
        if self.job.candidates.len() < limit
            && !value.is_empty()
            && value.chars().count() <= limits::CANDIDATE_CHARS
            && !value.chars().any(char::is_control)
            && !kanade_llm::profile::TSUMUGI.prompt.reserves(&value)
            && !self.job.candidates.contains(&value)
            && preserves_name_spelling(&self.job.candidates[0], &value)
        {
            self.job.candidates.push(value);
            self.penalties.push(penalty);
        }
    }
}

pub(super) fn widen(p: &mut Proposal, parts: &[Segment], lists: &[Vec<FocusCandidate>]) {
    for rank in 1..SEGMENT_DEPTH {
        for (pos, list) in lists.iter().enumerate() {
            if let Some(c) = list.get(rank)
                && !c.transliteration
            {
                let penalty = RANK_PENALTY * rank as f32;
                p.offer(
                    substitute(parts, pos, &c.value),
                    penalty,
                    limits::SENTENCE_POOL - SPLIT_RESERVE,
                );
            }
        }
    }
}

fn cuts(parts: &[Segment]) -> Vec<(u8, usize, usize)> {
    let mut choices = Vec::new();
    let mut learned = Vec::new();
    for (pos, part) in parts.iter().enumerate() {
        let kana: Vec<_> = part.reading.char_indices().collect();
        for i in 2..kana.len().saturating_sub(1) {
            let key = (u32::from(kana[i - 1].1) << 16) | u32::from(kana[i].1);
            if let Ok(j) = boundary_data::BOUNDARIES.binary_search_by_key(&key, |x| x.0) {
                choices.push((boundary_data::BOUNDARIES[j].1, pos, kana[i].0));
            }
            let pair = u64::from(key);
            let left = (u64::from(kana[i - 2].1) << 32) | pair;
            let right = (pair << 16) | u64::from(kana[i + 1].1);
            let probability = [
                (cut_data::PAIR, pair),
                (cut_data::LEFT, left),
                (cut_data::RIGHT, right),
            ]
            .into_iter()
            .filter_map(|(table, code)| {
                table
                    .binary_search_by_key(&code, |x| x.0)
                    .ok()
                    .map(|j| table[j].1)
            })
            .max();
            if let Some(p) = probability {
                learned.push((p, pos, kana[i].0));
            }
        }
    }
    choices.sort_by_key(|&(p, pos, i)| (std::cmp::Reverse(p), pos, i));
    choices.truncate(2);
    learned.sort_by_key(|&(p, pos, i)| (std::cmp::Reverse(p), pos, i));
    for cut in learned {
        if !choices
            .iter()
            .any(|&(_, pos, offset)| (pos, offset) == (cut.1, cut.2))
        {
            choices.push(cut);
            if choices.len() == 4 {
                break;
            }
        }
    }
    choices
}

pub(super) struct Split {
    pub(super) index: usize,
    pub(super) start: usize,
    pub(super) end: usize,
}

// Replace one original segment outside the probe's actual reading interval.
pub(super) fn extend_splits(
    p: &mut Proposal,
    parts: &[Segment],
    lists: &[Vec<FocusCandidate>],
    splits: &[Split],
) {
    let limit = (p.job.candidates.len() + 18).min(limits::SENTENCE_POOL);
    for span in splits {
        let split = p.job.candidates[span.index].clone();
        let mut left = 0;
        let mut right = split.len();
        let prefix = parts[..span.start].iter().enumerate().map(|(pos, part)| {
            let end = left + part.value.len();
            let range = left..end;
            left = end;
            (pos, range)
        });
        let suffix = parts
            .iter()
            .enumerate()
            .skip(span.end)
            .rev()
            .map(|(pos, part)| {
                let start = right - part.value.len();
                let range = start..right;
                right = start;
                (pos, range)
            });
        for (pos, range) in prefix.chain(suffix) {
            if p.job.candidates.len() >= limit {
                return;
            }
            if let Some(c) = lists[pos].get(1).filter(|c| !c.transliteration) {
                let mut text = split.clone();
                text.replace_range(range, &c.value);
                p.offer(text, p.penalties[span.index] + 2.0, limit);
            }
        }
    }
}

pub(super) fn paired(p: &mut Proposal, parts: &[Segment], lists: &[Vec<FocusCandidate>]) {
    let limit = (p.job.candidates.len() + 24).min(limits::SENTENCE_POOL);
    for &(distance, first_rank, second_rank) in edit_data::EDIT_PAIRS {
        for (first, list) in lists.iter().enumerate() {
            let Some(a) = list.get(first_rank).filter(|c| !c.transliteration) else {
                continue;
            };
            for (second, next) in lists.iter().enumerate().skip(first + distance) {
                if p.job.candidates.len() >= limit {
                    return;
                }
                if (second - first).min(3) != distance {
                    break;
                }
                let Some(b) = next.get(second_rank).filter(|c| !c.transliteration) else {
                    continue;
                };
                let value = parts
                    .iter()
                    .enumerate()
                    .map(|(i, part)| {
                        if i == first {
                            a.value.as_str()
                        } else if i == second {
                            b.value.as_str()
                        } else {
                            part.value.as_str()
                        }
                    })
                    .collect();
                p.offer(
                    value,
                    1.5 + (first_rank + second_rank) as f32 * RANK_PENALTY,
                    limit,
                );
            }
        }
    }
}

fn fragment<C: ConversionEngine>(
    backend: &mut C,
    probe: kanade_mozc::SessionId,
    kana: &str,
    context: &str,
) -> Option<Vec<String>> {
    backend.reset(probe).ok()?;
    backend.restore(probe, kana).ok()?;
    let snap = backend.key(probe, &key(SpecialKey::Space), context).ok()?;
    if !snap.converting
        || snap.commit.is_some()
        || snap.focused_segment != 0
        || reading(&snap.preedit) != kana
    {
        return None;
    }
    let original = &snap.preedit.segments;
    let mut values: Vec<String> = vec![original.iter().map(|s| s.value.as_str()).collect()];
    for c in snap
        .candidates
        .iter()
        .take(6)
        .filter(|c| !c.transliteration)
    {
        let word = substitute(original, 0, &c.value);
        if !values.contains(&word) {
            values.push(word);
        }
        if values.len() == 3 {
            break;
        }
    }
    Some(values)
}

pub(super) fn resegment<C: ConversionEngine>(
    backend: &mut C,
    probe: kanade_mozc::SessionId,
    parts: &[Segment],
    p: &mut Proposal,
    deadline: Instant,
) -> Vec<Split> {
    let cuts = cuts(parts);
    let mut splits = Vec::new();
    for (index, &(_, mut pos, offset)) in cuts.iter().enumerate() {
        if Instant::now() >= deadline {
            break;
        }
        let (a, b) = parts[pos].reading.split_at(offset);
        let (mut first, mut second) = (a.to_owned(), b.to_owned());
        let mut end = pos + 1;
        // For the second probe, let the adjacent segment share the new boundary.
        if index == 1 {
            if pos > 0 && (end == parts.len() || a.len() < b.len()) {
                pos -= 1;
                first.insert_str(0, &parts[pos].reading);
            } else if end < parts.len() {
                second.push_str(&parts[end].reading);
                end += 1;
            }
        }
        let Some(left) = fragment(backend, probe, &first, &p.job.context) else {
            continue;
        };
        if Instant::now() >= deadline {
            break;
        }
        let Some(right) = fragment(backend, probe, &second, "") else {
            continue;
        };
        for (i, a) in left.iter().enumerate() {
            for (j, b) in right.iter().enumerate() {
                let value = parts[..pos]
                    .iter()
                    .map(|s| s.value.as_str())
                    .chain([a.as_str(), b.as_str()])
                    .chain(parts[end..].iter().map(|s| s.value.as_str()))
                    .collect();
                let index = p.job.candidates.len();
                p.offer(
                    value,
                    4.0 + (i + j) as f32 * RANK_PENALTY,
                    limits::SENTENCE_POOL,
                );
                if p.job.candidates.len() > index {
                    splits.push(Split {
                        index,
                        start: pos,
                        end,
                    });
                }
            }
        }
    }
    splits
}
