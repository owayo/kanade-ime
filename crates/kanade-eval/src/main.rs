use anyhow::{Context, Result, ensure};
use kanade_ipc::{Key, Privacy, SpecialKey};
use kanade_mozc::{ConversionEngine, Mozc, SessionId, Snapshot};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Instant};

mod candidates;
mod romaji;

fn key(c: char) -> Key {
    Key {
        text: c.to_string(),
        ..Default::default()
    }
}
fn special(key: SpecialKey) -> Key {
    Key {
        special: key as i32,
        ..Default::default()
    }
}
fn type_text(mozc: &mut Mozc, id: SessionId, text: &str) -> Result<Snapshot> {
    let mut out = Snapshot::default();
    for c in text.chars() {
        out = mozc.key(id, &key(c), "")?;
    }
    Ok(out)
}
fn fresh(profile: &Path) -> Result<Mozc> {
    Ok(Mozc::start(&kanade_mozc::default_worker(), profile)?)
}
fn contract() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let profile = temp.path().join("profile");
    let mut mozc = fresh(&profile)?;
    mozc.set_privacy(Privacy::Normal)?;
    let id = mozc.create_session()?;
    let typed = type_text(&mut mozc, id, "nihongo")?;
    ensure!(
        kanade_ipc::reading(&typed.preedit) == "にほんご",
        "romaji preedit contract"
    );
    let converted = mozc.key(id, &special(SpecialKey::Space), "")?;
    ensure!(!converted.candidates.is_empty(), "candidate contract");
    ensure!(
        converted.candidates.iter().any(|c| c.value == "日本語"),
        "known conversion contract"
    );
    mozc.select(id, "日本語")?;
    let committed = mozc.submit(id)?;
    let commit = committed.commit.context("submit did not return a result")?;
    ensure!(
        commit.value == "日本語" && commit.reading == "にほんご",
        "select + submit contract"
    );
    ensure!(!commit.words.is_empty(), "result token contract");
    let undone = mozc.key(id, &special(SpecialKey::Backspace), "日本語")?;
    ensure!(
        !undone.consumed,
        "Backspace must pass through after a commit"
    );
    type_text(&mut mozc, id, "kyouhaiitenkidesu")?;
    let multi = mozc.key(id, &special(SpecialKey::Space), "")?;
    ensure!(
        multi.preedit.segments.len() > 1,
        "multiple segments contract"
    );
    let moved = mozc.key(id, &special(SpecialKey::Right), "")?;
    ensure!(
        !moved.preedit.segments.is_empty(),
        "focus move must preserve preedit"
    );
    mozc.reset(id)?;
    mozc.sync()?;
    mozc.reload()?;
    mozc.delete_session(id)?;
    ensure!(
        mozc.key(id, &key('a'), "").is_err(),
        "expired session must fail"
    );
    mozc.shutdown()?;
    drop(mozc);
    let mut mozc = fresh(&profile)?;
    mozc.set_privacy(Privacy::Private)?;
    let id = mozc.create_session()?;
    type_text(&mut mozc, id, "nihongo")?;
    let out = mozc.key(id, &special(SpecialKey::Space), "")?;
    ensure!(
        !out.candidates.is_empty(),
        "private input must still convert"
    );
    mozc.reset(id)?;
    mozc.delete_session(id)?;
    mozc.shutdown()?;
    println!(
        "{{\"contract\":\"passed\",\"checks\":[\"romaji\",\"candidates\",\"select-submit\",\"result-tokens\",\"undo-pass-through\",\"segments\",\"focus\",\"reset\",\"sync-reload\",\"expired-session\",\"restart\",\"private-input\"]}}"
    );
    Ok(())
}
#[derive(Deserialize)]
struct Case {
    romaji: String,
    accepted: Vec<String>,
    domain: String,
    #[serde(default)]
    preceding: String,
}
#[derive(Default, Serialize)]
struct Score {
    cases: u32,
    correct: u32,
    top1: f64,
    top9_correct: u32,
    top9: f64,
    missed_cases: Vec<usize>,
    not_in_top9_cases: Vec<usize>,
}
fn evaluate(path: &Path) -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut mozc = fresh(temp.path())?;
    mozc.set_privacy(Privacy::Private)?;
    let mut domains = std::collections::BTreeMap::<String, Score>::new();
    for (row, line) in std::fs::read_to_string(path)?
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
    {
        let case: Case = serde_json::from_str(line)?;
        let id = mozc.create_session()?;
        let mut committed = String::new();
        for c in case.romaji.chars() {
            let out = mozc.key(id, &key(c), &case.preceding)?;
            if let Some(commit) = out.commit {
                committed.push_str(&commit.value);
            }
            if !out.consumed {
                committed.push(c);
            }
        }
        let out = mozc.key(id, &special(SpecialKey::Space), &case.preceding)?;
        if let Some(commit) = &out.commit {
            committed.push_str(&commit.value);
        }
        let value = format!("{committed}{}", kanade_ipc::surface(&out.preedit));
        let correct = case.accepted.contains(&value);
        let in_top9 = correct
            || out.candidates.iter().take(9).any(|candidate| {
                let mut value = committed.clone();
                for (i, segment) in out.preedit.segments.iter().enumerate() {
                    value.push_str(if i == out.focused_segment {
                        &candidate.value
                    } else {
                        &segment.value
                    });
                }
                case.accepted.contains(&value)
            });
        let score = domains.entry(case.domain).or_default();
        score.cases += 1;
        if correct {
            score.correct += 1;
        } else {
            // Identify fixture rows without logging the supplied input or output.
            score.missed_cases.push(row + 1);
        }
        if in_top9 {
            score.top9_correct += 1;
        } else {
            score.not_in_top9_cases.push(row + 1);
        }
        mozc.reset(id)?;
        mozc.delete_session(id)?;
    }
    for score in domains.values_mut() {
        score.top1 = score.correct as f64 / score.cases as f64;
        score.top9 = score.top9_correct as f64 / score.cases as f64;
    }
    mozc.shutdown()?;
    println!("{}", serde_json::to_string_pretty(&domains)?);
    Ok(())
}
fn bench() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut mozc = fresh(temp.path())?;
    mozc.set_privacy(Privacy::Private)?;
    let id = mozc.create_session()?;
    let mut times = vec![];
    for _ in 0..100 {
        for c in "nihongo".chars() {
            let start = Instant::now();
            mozc.key(id, &key(c), "")?;
            times.push(start.elapsed().as_micros() as u64);
        }
        let start = Instant::now();
        mozc.key(id, &special(SpecialKey::Space), "")?;
        times.push(start.elapsed().as_micros() as u64);
        mozc.reset(id)?;
    }
    times.sort_unstable();
    let n = times.len();
    println!(
        "{}",
        serde_json::json!({"metric":"worker_round_trip_us","samples":n,"p50":times[n/2],"p95":times[n*95/100],"p99":times[n*99/100],"max":times[n-1],"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"profile":"private, warm"})
    );
    mozc.delete_session(id)?;
    mozc.shutdown()?;
    Ok(())
}
fn convert(romaji: &str) -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut mozc = fresh(temp.path())?;
    mozc.set_privacy(Privacy::Private)?;
    let id = mozc.create_session()?;
    type_text(&mut mozc, id, romaji)?;
    let out = mozc.key(id, &special(SpecialKey::Space), "")?;
    println!(
        "{}",
        serde_json::json!({"preedit":out.preedit,"candidates":out.candidates.iter().take(9).map(|c|&c.value).collect::<Vec<_>>()})
    );
    mozc.reset(id)?;
    mozc.delete_session(id)?;
    mozc.shutdown()?;
    Ok(())
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("contract") => contract(),
        Some("bench") => bench(),
        Some("evaluate") => evaluate(Path::new(
            args.get(2)
                .map(String::as_str)
                .unwrap_or("eval/conversion.jsonl"),
        )),
        Some("convert") => convert(args.get(2).context("romaji is required")?),
        Some("candidates") => candidates::run(
            candidates::Options::parse(&args[2..])?,
            std::io::stdin().lock(),
            std::io::stdout().lock(),
        ),
        _ => anyhow::bail!(
            "usage: kanade-eval <contract|bench|evaluate [dataset]|convert ROMAJI|candidates [--limit N] [--max-reading N] [--deadline-ms N] [--suggest]>"
        ),
    }
}
