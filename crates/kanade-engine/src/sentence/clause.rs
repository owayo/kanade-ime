// SPDX-License-Identifier: MIT
//! Score one kana clause while retaining all punctuation and surrounding text.
use super::*;

#[derive(Default)]
struct Clause {
    segments: Vec<Segment>,
    lists: Vec<Vec<FocusCandidate>>,
    surface: String,
    reading: String,
    separator: Option<char>,
}

fn fields(text: &str) -> Vec<(&str, Option<char>)> {
    let mut result = Vec::new();
    let mut start = 0;
    for (end, mark) in text.match_indices(['、', '。']) {
        result.push((&text[start..end], mark.chars().next()));
        start = end + mark.len();
    }
    result.push((&text[start..], None));
    result
}

pub(super) fn proposal(
    parts: &[Segment],
    lists: &[Vec<FocusCandidate>],
    job: &Job,
    resegment: impl FnOnce(&[Segment], &mut Proposal),
) -> Option<Proposal> {
    let mut clauses = vec![Clause::default()];
    for (part, list) in parts.iter().zip(lists) {
        let displayed = fields(&part.value);
        let reading = fields(&part.reading);
        if displayed.len() != reading.len() {
            return None;
        }
        let candidates: Vec<_> = list.iter().map(|c| (c, fields(&c.value))).collect();
        for (i, ((surface, mark), (kana, kana_mark))) in displayed.iter().zip(reading).enumerate() {
            if *mark != kana_mark || surface.is_empty() != kana.is_empty() {
                return None;
            }
            let clause = clauses.last_mut()?;
            if !surface.is_empty() {
                let mut alternatives = Vec::<FocusCandidate>::new();
                for (candidate, words) in &candidates {
                    if words.len() != displayed.len()
                        || words[..i] != displayed[..i]
                        || words[i + 1..] != displayed[i + 1..]
                        || words[i].1 != *mark
                    {
                        continue;
                    }
                    let text = words[i].0;
                    if !text.is_empty() && !alternatives.iter().any(|a| a.value == text) {
                        let mut entry = (*candidate).clone();
                        entry.value = text.to_owned();
                        alternatives.push(entry);
                    }
                }
                clause.surface.push_str(surface);
                clause.reading.push_str(kana);
                clause.segments.push(Segment {
                    value: (*surface).to_owned(),
                    reading: kana.to_owned(),
                    ..Default::default()
                });
                clause.lists.push(alternatives);
            }
            if mark.is_some() {
                clause.separator = *mark;
                clauses.push(Clause::default());
            }
        }
    }
    let whole: String = parts.iter().map(|p| p.value.as_str()).collect();
    let mut offset = 0;
    let mut order = Vec::new();
    for cl in &clauses {
        let end = offset + cl.surface.len();
        if (1..=8).contains(&cl.segments.len())
            && cl.surface.chars().count() <= limits::CANDIDATE_CHARS
        {
            order.push((cl, offset, end));
        }
        offset = end + cl.separator.map_or(0, char::len_utf8);
    }
    order.sort_by_key(|(c, _, _)| std::cmp::Reverse(c.reading.chars().count()));
    let mut split = Some(resegment);
    for (cl, start, end) in order {
        let before = &whole[..start];
        let Some(context) = kanade_llm::context_window(&format!("{}{before}", job.context)) else {
            continue;
        };
        let local = Job {
            context,
            reading: kanade_llm::katakana(&cl.reading),
            ..job.clone()
        };
        let mut item = contextual_proposal(&cl.segments, &cl.lists, local);
        if let Some(probe) = split.take() {
            probe(&cl.segments, &mut item);
        }
        if item.job.valid_for(true) {
            item.before = before.to_owned();
            item.after = whole[end..].to_owned();
            item.margin = 7.0;
            return Some(item);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    fn part(reading: &str, value: &str) -> Segment {
        Segment {
            reading: reading.into(),
            value: value.into(),
            ..Default::default()
        }
    }
    fn option(value: &str) -> FocusCandidate {
        FocusCandidate {
            reading: String::new(),
            annotation: String::new(),
            value: value.into(),
            transliteration: false,
            history_deletable: false,
        }
    }
    #[test]
    fn a_clause_can_change_without_moving_punctuation_or_its_neighbors() {
        let parts = [part("さて、けんとう。", "さて、検討。")];
        let lists = vec![vec![
            option("さて、検討。"),
            option("さて、見当。"),
            option("さて見当。"),
            option("サテ、見当。"),
        ]];
        let job = Job {
            context: "そうして".into(),
            reading: "サテ、ケントウ。".into(),
            prefix: String::new(),
            suffix: String::new(),
            candidates: vec![],
        };
        let choice = proposal(&parts, &lists, &job, |_, _| {}).unwrap();
        assert_eq!(choice.job.reading, "ケントウ");
        assert_eq!(choice.job.context, "そうしてさて、");
        assert_eq!(choice.job.candidates, ["検討", "見当"]);
        assert_eq!(choice.render(1), "さて、見当。");
    }
}
