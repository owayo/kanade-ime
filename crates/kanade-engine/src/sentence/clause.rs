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

// One proposal for each window of each clause, the longest clause first.
pub(super) fn proposals(
    parts: &[Segment],
    lists: &[Vec<FocusCandidate>],
    job: &Job,
    policy: &Preview,
    resegment: impl FnOnce(&[Segment], &mut Proposal) -> Vec<search::Split>,
) -> Vec<Proposal> {
    split(parts, lists, job, policy, resegment).unwrap_or_default()
}

fn split(
    parts: &[Segment],
    lists: &[Vec<FocusCandidate>],
    job: &Job,
    policy: &Preview,
    resegment: impl FnOnce(&[Segment], &mut Proposal) -> Vec<search::Split>,
) -> Option<Vec<Proposal>> {
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
        if !cl.segments.is_empty() {
            order.push((cl, offset, end));
        }
        offset = end + cl.separator.map_or(0, char::len_utf8);
    }
    order.sort_by_key(|(c, _, _)| std::cmp::Reverse(c.reading.chars().count()));
    let mut probe = Some(resegment);
    let mut found = Vec::new();
    for (cl, start, end) in order {
        if found.len() >= MAX_WINDOWS {
            break;
        }
        let before = &whole[..start];
        let preceding = format!("{}{before}", job.context);
        let context = if preceding.is_empty() {
            String::new()
        } else if let Some(context) = kanade_llm::context_window(&preceding) {
            context
        } else {
            continue;
        };
        let local = Job {
            context,
            reading: kanade_llm::katakana(&cl.reading),
            ..job.clone()
        };
        let windows = scored_windows(
            &cl.segments,
            &cl.lists,
            &local,
            MAX_WINDOWS - found.len(),
            policy,
            |parts, item| {
                probe
                    .take()
                    .map_or_else(Vec::new, |probe| probe(parts, item))
            },
        );
        for mut item in windows {
            item.before.insert_str(0, before);
            item.after.push_str(&whole[end..]);
            item.margin = policy.clause;
            found.push(item);
        }
    }
    Some(found)
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
        // The first clause has no alternative of its own and is not scored.
        let found = proposals(
            &parts,
            &lists,
            &job,
            &crate::sentence::TEST_POLICY,
            |_, _| Vec::new(),
        );
        assert_eq!(found.len(), 1);
        let choice = &found[0];
        assert_eq!(choice.job.reading, "ケントウ");
        assert_eq!(choice.job.context, "そうしてさて、");
        assert_eq!(choice.job.candidates, ["検討", "見当"]);
        assert_eq!(choice.render(1), "さて、見当。");
    }

    #[test]
    fn displayed_prefix_supplies_missing_context_and_survives_a_second_edit() {
        let parts = [
            part("そっと", "そっと"),
            part("きに", "木に"),
            part("わしを", "和紙を"),
            part("はる", "貼る"),
        ];
        let lists = [
            vec![option("そっと")],
            vec![option("木に"), option("気に")],
            vec![option("和紙を")],
            vec![option("貼る"), option("張る")],
        ];
        let job = Job {
            context: String::new(),
            reading: "ソットキニワシヲハル".into(),
            prefix: String::new(),
            suffix: String::new(),
            candidates: vec![],
        };
        let mut found = scored_windows(
            &parts,
            &lists,
            &job,
            MAX_WINDOWS,
            &crate::sentence::TEST_POLICY,
            |window, p| {
                assert_eq!(window[0].value, "木に");
                p.job.candidates.push("木に和詩を貼る".into());
                p.penalties.push(4.0);
                p.changes.push(None);
                vec![search::Split {
                    index: p.penalties.len() - 1,
                    start: 1,
                    end: 2,
                }]
            },
        );
        assert_eq!(found.len(), 1);
        let item = found.pop().unwrap();
        assert_eq!(item.job.context, "そっと");
        assert_eq!(item.job.reading, "キニワシヲハル");
        for expected in ["そっと気に和詩を貼る", "そっと木に和詩を張る"] {
            assert!((0..item.job.candidates.len()).any(|i| item.render(i) == expected));
        }
    }

    #[test]
    fn every_clause_is_offered_with_the_text_displayed_around_it() {
        let parts = [
            part("にんきは、", "人気は、"),
            part("にねんと", "二年と"),
            part("する。", "する。"),
        ];
        let lists = [
            vec![option("人気は、"), option("任期は、")],
            vec![option("二年と"), option("2年と")],
            vec![option("する。")],
        ];
        let job = Job {
            context: "委員の".into(),
            reading: "ニンキハ、ニネントスル。".into(),
            prefix: String::new(),
            suffix: String::new(),
            candidates: vec![],
        };
        let found = proposals(
            &parts,
            &lists,
            &job,
            &crate::sentence::TEST_POLICY,
            |_, _| Vec::new(),
        );
        let whole = "人気は、二年とする。";
        // The longer clause comes first; both render the unchanged text by default.
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|item| item.render(0) == whole));
        assert_eq!(found[0].job.reading, "ニネントスル");
        assert_eq!(found[0].job.context, "委員の人気は、");
        assert_eq!(found[0].job.candidates, ["二年とする", "2年とする"]);
        assert_eq!(
            (found[0].before.as_str(), found[0].after.as_str()),
            ("人気は、", "。")
        );
        assert_eq!(found[1].job.reading, "ニンキハ");
        assert_eq!(found[1].job.context, "委員の");
        assert_eq!(found[1].job.candidates, ["人気は", "任期は"]);
        assert_eq!(found[1].after, "、二年とする。");
        assert_eq!(found[1].render(1), "任期は、二年とする。");
    }
}
