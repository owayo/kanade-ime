use kanade_ipc::{Key, Privacy, SpecialKey};
use kanade_mozc::{ConversionEngine, LearningKind, Mozc, SessionId, Snapshot};

fn type_text(mozc: &mut Mozc, id: SessionId, text: &str) -> Snapshot {
    let mut result = Snapshot::default();
    for text in text.chars() {
        result = mozc
            .key(
                id,
                &Key {
                    text: text.into(),
                    ..Default::default()
                },
                "",
            )
            .unwrap();
    }
    result
}

fn conversion(mozc: &mut Mozc, id: SessionId) -> Snapshot {
    type_text(mozc, id, "kami");
    mozc.key(
        id,
        &Key {
            special: SpecialKey::Space as i32,
            ..Default::default()
        },
        "",
    )
    .unwrap()
}

#[test]
#[ignore = "requires the locked Mozc artifact; make contract"]
fn separate_clears_survive_restart_and_preserve_other_learning() {
    let profile = tempfile::tempdir().unwrap();
    let mut mozc = Mozc::start(&kanade_mozc::default_worker(), profile.path()).unwrap();
    mozc.set_privacy(Privacy::Normal).unwrap();
    let id = mozc.create_session().unwrap();
    let initial = conversion(&mut mozc, id);
    let baseline = initial.preedit.segments[0].value.clone();
    let learned = initial
        .candidates
        .iter()
        .find(|candidate| candidate.value != baseline)
        .unwrap()
        .value
        .clone();
    mozc.reset(id).unwrap();
    for _ in 0..4 {
        conversion(&mut mozc, id);
        mozc.select(id, &learned).unwrap();
        mozc.submit(id).unwrap();
    }
    mozc.set_privacy(Privacy::Unknown).unwrap();
    mozc.clear_learning(LearningKind::Prediction).unwrap();
    mozc.restart().unwrap();
    mozc.set_privacy(Privacy::Normal).unwrap();
    let id = mozc.create_session().unwrap();
    assert_eq!(conversion(&mut mozc, id).preedit.segments[0].value, learned);
    mozc.reset(id).unwrap();
    mozc.set_privacy(Privacy::Unknown).unwrap();
    mozc.clear_learning(LearningKind::Conversion).unwrap();
    mozc.restart().unwrap();
    mozc.set_privacy(Privacy::Normal).unwrap();
    let id = mozc.create_session().unwrap();
    assert_eq!(
        conversion(&mut mozc, id).preedit.segments[0].value,
        baseline
    );
    mozc.reset(id).unwrap();

    mozc.set_privacy(Privacy::Normal).unwrap();
    mozc.add_history("かなでてすと", "KanadeHistoryFixture")
        .unwrap();
    mozc.set_privacy(Privacy::Unknown).unwrap();
    mozc.clear_learning(LearningKind::Conversion).unwrap();
    mozc.restart().unwrap();
    mozc.set_privacy(Privacy::Normal).unwrap();
    let id = mozc.create_session().unwrap();
    assert!(
        type_text(&mut mozc, id, "kanadetesuto")
            .candidates
            .iter()
            .any(|candidate| candidate.value == "KanadeHistoryFixture")
    );
    mozc.reset(id).unwrap();
    mozc.set_privacy(Privacy::Unknown).unwrap();
    mozc.clear_learning(LearningKind::Prediction).unwrap();
    mozc.restart().unwrap();
    mozc.set_privacy(Privacy::Normal).unwrap();
    let id = mozc.create_session().unwrap();
    assert!(
        type_text(&mut mozc, id, "kanadetesuto")
            .candidates
            .iter()
            .all(|candidate| candidate.value != "KanadeHistoryFixture")
    );
}

#[test]
#[ignore = "requires the locked Mozc artifact; make contract"]
fn explicit_candidate_deletion_preserves_other_history_and_rejects_stale_targets() {
    let profile = tempfile::tempdir().unwrap();
    let mut mozc = Mozc::start(&kanade_mozc::default_worker(), profile.path()).unwrap();
    mozc.set_privacy(Privacy::Normal).unwrap();
    let id = mozc.create_session().unwrap();
    let initial = conversion(&mut mozc, id);
    let baseline = initial.preedit.segments[0].value.clone();
    let learned = initial
        .candidates
        .iter()
        .find(|c| !c.transliteration && c.value != baseline)
        .unwrap()
        .value
        .clone();
    mozc.reset(id).unwrap();
    for _ in 0..4 {
        conversion(&mut mozc, id);
        mozc.select(id, &learned).unwrap();
        mozc.submit(id).unwrap();
    }
    mozc.add_history("かなでてすと", "KanadeHistoryFixture")
        .unwrap();
    let before = conversion(&mut mozc, id);
    assert_eq!(before.preedit.segments[0].value, learned);
    assert!(matches!(
        mozc.delete_candidate_history(id, "MissingFixture"),
        Err(kanade_mozc::Error::StaleCandidate)
    ));
    // Resolve the named target even while another candidate is selected.
    mozc.select(id, &baseline).unwrap();
    assert!(matches!(
        mozc.delete_candidate_history(id, &baseline),
        Err(kanade_mozc::Error::StaleCandidate)
    ));
    let after = mozc.delete_candidate_history(id, &learned).unwrap();
    assert!(after.consumed);
    assert!(after.commit.is_none());
    assert_eq!(kanade_ipc::reading(&after.preedit), "かみ");
    assert!(!after.converting);
    assert!(matches!(
        mozc.delete_candidate_history(id, &learned),
        Err(kanade_mozc::Error::StaleCandidate)
    ));
    mozc.sync().unwrap();
    mozc.restart().unwrap();
    mozc.set_privacy(Privacy::Normal).unwrap();
    let id = mozc.create_session().unwrap();
    assert_eq!(
        conversion(&mut mozc, id).preedit.segments[0].value,
        baseline
    );
    mozc.reset(id).unwrap();
    assert!(
        type_text(&mut mozc, id, "kanadetesuto")
            .candidates
            .iter()
            .any(|c| c.value == "KanadeHistoryFixture")
    );
}

#[test]
#[ignore = "requires the locked Mozc artifact; make contract"]
fn candidate_deletion_stays_within_the_deadline_after_ten_thousand_history_insertions() {
    use std::time::Instant;
    let profile = tempfile::tempdir().unwrap();
    let mut mozc = Mozc::start(&kanade_mozc::default_worker(), profile.path()).unwrap();
    mozc.set_privacy(Privacy::Normal).unwrap();
    let id = mozc.create_session().unwrap();
    for i in 0..10_000 {
        mozc.add_history(&format!("のきろく{i}"), &format!("HistoryBenchmark{i}"))
            .unwrap();
    }
    let mut micros = Vec::new();
    for _ in 0..100 {
        mozc.add_history("かなでそくど", "KanadeLatencyFixture")
            .unwrap();
        type_text(&mut mozc, id, "kanadesokudo");
        let predicted = mozc
            .key(
                id,
                &Key {
                    special: SpecialKey::Tab as i32,
                    ..Default::default()
                },
                "",
            )
            .unwrap();
        assert!(
            predicted
                .candidates
                .iter()
                .any(|c| c.value == "KanadeLatencyFixture" && c.history_deletable)
        );
        let start = Instant::now();
        let result = mozc
            .delete_candidate_history(id, "KanadeLatencyFixture")
            .unwrap();
        micros.push(start.elapsed().as_micros());
        assert!(result.commit.is_none());
        assert!(!result.converting);
        mozc.reset(id).unwrap();
    }
    micros.sort_unstable();
    println!(
        "candidate-deletion: insertions=10000 samples=100 p50_us={} p99_us={} max_us={}",
        micros[49], micros[98], micros[99]
    );
}
