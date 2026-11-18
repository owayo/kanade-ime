use kanade_engine::Engine;
use kanade_ipc::*;
use kanade_mozc::Mozc;
fn engine() -> (tempfile::TempDir, Engine<Mozc>, u64, Revision) {
    let temp = tempfile::tempdir().unwrap();
    let backend = Mozc::start(&kanade_mozc::default_worker(), temp.path()).unwrap();
    let mut engine = Engine::new(backend, None);
    let w = engine
        .open(&Hello {
            min_version: 1,
            max_version: 2,
        })
        .unwrap();
    let rev = Revision {
        connection: w.connection,
        context: 1,
        composition: 0,
    };
    (temp, engine, w.connection, rev)
}
fn input(revision: Revision, seq: u64, text: &str) -> Request {
    Request {
        body: Some(request::Body::Key(KeyRequest {
            revision: Some(revision),
            event_seq: seq,
            key: Some(Key {
                text: text.into(),
                ..Default::default()
            }),
            context: Some(Context {
                privacy: Privacy::Private as i32,
                ..Default::default()
            }),
            passed_through: false,
        })),
    }
}
fn reply(response: Option<Response>) -> KeyReply {
    match response.unwrap().body.unwrap() {
        response::Body::Key(reply) => reply,
        _ => panic!("expected key reply"),
    }
}
fn ack(reply: &KeyReply, applied: bool) -> Request {
    Request {
        body: Some(request::Body::Ack(Ack {
            revision: reply.revision,
            event_seq: reply.event_seq,
            applied,
        })),
    }
}
#[test]
#[ignore = "requires the locked Mozc artifact; mise run contract"]
fn repeated_event_is_not_sent_to_mozc_twice() {
    let (_temp, mut e, id, rev) = engine();
    let request = input(rev, 1, "a");
    let first = reply(e.handle(id, request.clone()).unwrap());
    let again = reply(e.handle(id, request).unwrap());
    assert_eq!(first, again);
    e.handle(id, ack(&first, true)).unwrap();
    e.handle(id, ack(&first, true)).unwrap();
    let second = reply(
        e.handle(id, input(first.revision.unwrap(), 2, "a"))
            .unwrap(),
    );
    assert_eq!(reading(second.preedit.as_ref().unwrap()), "ああ");
}
#[test]
#[ignore = "requires the locked Mozc artifact; mise run contract"]
fn shifted_tilde_is_consumed_and_committed_once() {
    // The locked Mozc worker normalizes U+301C to U+FF5E on Windows.
    let tilde = if cfg!(windows) { "～" } else { "〜" };
    let (_temp, mut e, id, rev) = engine();
    let mut typed_request = input(rev, 1, "~");
    let Some(request::Body::Key(key)) = typed_request.body.as_mut() else {
        unreachable!()
    };
    key.key.as_mut().unwrap().modifiers = MOD_SHIFT;
    let typed = reply(e.handle(id, typed_request.clone()).unwrap());
    assert!(typed.consumed);
    assert_eq!(surface(typed.preedit.as_ref().unwrap()), tilde);
    assert_eq!(reply(e.handle(id, typed_request).unwrap()), typed);
    e.handle(id, ack(&typed, true)).unwrap();
    let mut submit = input(typed.revision.unwrap(), 2, "");
    let Some(request::Body::Key(key)) = submit.body.as_mut() else {
        unreachable!()
    };
    key.key.as_mut().unwrap().special = SpecialKey::Enter as i32;
    let committed = reply(e.handle(id, submit.clone()).unwrap());
    assert_eq!(committed.commit.as_ref().unwrap().value, tilde);
    assert_eq!(reply(e.handle(id, submit).unwrap()), committed);
    e.handle(id, ack(&committed, true)).unwrap();
    let next = reply(
        e.handle(id, input(committed.revision.unwrap(), 3, "a"))
            .unwrap(),
    );
    assert!(next.commit.is_none());
    assert_eq!(surface(next.preedit.as_ref().unwrap()), "あ");
}
#[test]
#[ignore = "requires the locked Mozc artifact; mise run contract"]
fn unacknowledged_edits_and_stale_contexts_are_rejected() {
    let (_temp, mut e, id, rev) = engine();
    let first = reply(e.handle(id, input(rev, 1, "a")).unwrap());
    assert!(
        e.handle(id, input(first.revision.unwrap(), 2, "a"))
            .is_err()
    );
    assert!(e.handle(id, input(rev, 1, "b")).is_err());
    e.handle(id, ack(&first, true)).unwrap();
    assert!(e.handle(id, input(rev, 2, "a")).is_err());
    e.handle(
        id,
        Request {
            body: Some(request::Body::Reset(Reset { context: 2 })),
        },
    )
    .unwrap();
    assert!(
        e.handle(id, input(first.revision.unwrap(), 2, "a"))
            .is_err()
    );
}
#[test]
#[ignore = "requires the locked Mozc artifact; make contract"]
fn mozc_clear_refuses_unacknowledged_and_active_composition() {
    use kanade_mozc::LearningKind;
    let (_temp, mut e, id, rev) = engine();
    let first = reply(e.handle(id, input(rev, 1, "a")).unwrap());
    assert_eq!(
        e.clear_mozc_learning(LearningKind::Conversion),
        Err(failure::Code::Sequence)
    );
    e.handle(id, ack(&first, true)).unwrap();
    assert_eq!(
        e.clear_mozc_learning(LearningKind::Prediction),
        Err(failure::Code::Sequence)
    );
    e.handle(
        id,
        Request {
            body: Some(request::Body::Reset(Reset { context: 2 })),
        },
    )
    .unwrap();
    e.clear_mozc_learning(LearningKind::Conversion).unwrap();
    e.clear_mozc_learning(LearningKind::Prediction).unwrap();
    let next = reply(
        e.handle(
            id,
            input(
                Revision {
                    connection: id,
                    context: 2,
                    composition: 0,
                },
                1,
                "i",
            ),
        )
        .unwrap(),
    );
    assert_eq!(reading(next.preedit.as_ref().unwrap()), "い");
}
#[test]
#[ignore = "requires the locked Mozc artifact; mise run contract"]
fn failed_document_application_ends_the_old_session() {
    let (_temp, mut e, id, rev) = engine();
    let first = reply(e.handle(id, input(rev, 1, "a")).unwrap());
    assert!(e.handle(id, ack(&first, false)).is_err());
    assert!(
        e.handle(id, input(first.revision.unwrap(), 2, "a"))
            .is_err()
    );
    e.recover_worker().unwrap();
    let welcome = e
        .open(&Hello {
            min_version: 1,
            max_version: 2,
        })
        .unwrap();
    assert_ne!(welcome.connection, id);
}

#[test]
#[ignore = "requires the locked Mozc artifact; mise run contract"]
fn synthetic_candidates_keep_click_and_digit_selection_consistent() {
    use kanade_indexer::{Catalog, Generation, Project, Registry, Term, write_json};
    use std::{collections::BTreeMap, sync::Arc};
    for (click, revoked, implicit) in [
        (true, false, false),
        (false, false, false),
        (true, true, false),
        (false, true, false),
        (false, false, true),
    ] {
        let (temp, mut e, id, mut revision) = engine();
        let data = temp.path().join("projects");
        Registry {
            revision: 0,
            projects: vec![Project {
                id: "sample".into(),
                approval: 0,
                root: temp.path().into(),
                apps: vec!["editor".into()],
                enabled: true,
                monitoring: false,
                directory_binding: None,
                readings: Default::default(),
            }],
        }
        .save(&data)
        .unwrap();
        let folder = data.join("sample");
        write_json(
            &folder.join("generation-1.json"),
            &Generation {
                generation: 1,
                approval: 0,
                files: BTreeMap::from([(
                    "sample.rs".into(),
                    vec![Term {
                        reading: "かなで".into(),
                        value: "奏IME".into(),
                    }],
                )]),
            },
        )
        .unwrap();
        write_json(&folder.join("current.json"), &1u64).unwrap();
        e.set_projects(Arc::new(Catalog::load(&data).unwrap()));
        e.configure(kanade_engine::config::Config {
            project_dictionary: true,
            ..Default::default()
        });
        let mut seq = 0;
        let mut latest = KeyReply::default();
        for ch in "kanade ".chars() {
            seq += 1;
            let mut request = input(revision, seq, &ch.to_string());
            if let Some(request::Body::Key(k)) = request.body.as_mut() {
                k.context.as_mut().unwrap().app = "editor".into();
                k.context.as_mut().unwrap().privacy = Privacy::Normal as i32;
                if ch == ' ' {
                    k.key.as_mut().unwrap().text.clear();
                    k.key.as_mut().unwrap().special = SpecialKey::Space as i32;
                }
            }
            latest = reply(e.handle(id, request).unwrap());
            revision = latest.revision.unwrap();
            e.handle(id, ack(&latest, true)).unwrap();
        }
        let synthetic = latest
            .candidates
            .as_ref()
            .unwrap()
            .items
            .iter()
            .find(|i| i.source == CandidateSource::Project as i32)
            .unwrap()
            .handle;
        seq += 1;
        latest = reply(
            e.handle(
                id,
                Request {
                    body: Some(request::Body::Select(SelectRequest {
                        revision: Some(revision),
                        event_seq: seq,
                        handle: synthetic,
                        submit: false,
                    })),
                },
            )
            .unwrap(),
        );
        revision = latest.revision.unwrap();
        e.handle(id, ack(&latest, true)).unwrap();
        let preview = latest.preedit.clone();
        seq += 1;
        latest = reply(
            e.handle(
                id,
                Request {
                    body: Some(request::Body::Key(KeyRequest {
                        revision: Some(revision),
                        event_seq: seq,
                        key: Some(Key {
                            special: SpecialKey::Delete as i32,
                            modifiers: MOD_CONTROL,
                            ..Default::default()
                        }),
                        context: Some(Context {
                            privacy: Privacy::Normal as i32,
                            app: "editor".into(),
                            ..Default::default()
                        }),
                        passed_through: false,
                    })),
                },
            )
            .unwrap(),
        );
        assert_eq!(latest.preedit, preview);
        assert!(latest.commit.is_none());
        revision = latest.revision.unwrap();
        e.handle(id, ack(&latest, true)).unwrap();
        let item = latest
            .candidates
            .as_ref()
            .unwrap()
            .items
            .iter()
            .find(|i| i.source == CandidateSource::Project as i32)
            .unwrap();
        seq += 1;
        let request = if implicit {
            let mut request = input(revision, seq, "a");
            if let Some(request::Body::Key(key)) = request.body.as_mut() {
                key.context.as_mut().unwrap().app = "editor".into();
                key.context.as_mut().unwrap().privacy = Privacy::Normal as i32;
            }
            request
        } else if click {
            Request {
                body: Some(request::Body::Select(SelectRequest {
                    revision: Some(revision),
                    event_seq: seq,
                    handle: item.handle,
                    submit: true,
                })),
            }
        } else {
            let mut request = input(revision, seq, &item.number.to_string());
            if let Some(request::Body::Key(key)) = request.body.as_mut() {
                key.context.as_mut().unwrap().app = "editor".into();
                key.context.as_mut().unwrap().privacy = Privacy::Normal as i32;
            }
            request
        };
        if revoked {
            e.set_projects(Arc::new(Catalog::default()));
            assert_eq!(e.handle(id, request), Err(failure::Code::Stale));
            continue;
        }
        let committed = reply(e.handle(id, request.clone()).unwrap());
        assert_eq!(committed.commit.as_ref().unwrap().value, "奏IME");
        assert!(committed.commit.as_ref().unwrap().synthetic);
        assert_eq!(committed, reply(e.handle(id, request).unwrap()));
        e.handle(id, ack(&committed, true)).unwrap();
        let next = reply(
            e.handle(id, input(committed.revision.unwrap(), seq + 1, "a"))
                .unwrap(),
        );
        assert_eq!(
            reading(next.preedit.as_ref().unwrap()),
            if implicit { "ああ" } else { "あ" }
        );
    }
}

struct Editor {
    engine: Engine<Mozc>,
    id: u64,
    revision: Revision,
    seq: u64,
    document: String,
    last: KeyReply,
}
impl Editor {
    fn key(&mut self, text: &str, special: SpecialKey) {
        self.modified_key(text, special, 0);
    }
    fn modified_key(&mut self, text: &str, special: SpecialKey, modifiers: u32) {
        self.seq += 1;
        let response = self
            .engine
            .handle(
                self.id,
                Request {
                    body: Some(request::Body::Key(KeyRequest {
                        revision: Some(self.revision),
                        event_seq: self.seq,
                        key: Some(Key {
                            text: text.into(),
                            special: special as i32,
                            modifiers,
                        }),
                        context: Some(Context {
                            privacy: Privacy::Normal as i32,
                            app: "com.apple.TextEdit".into(),
                            preceding_text: self.document.clone(),
                            surrounding_available: true,
                        }),
                        passed_through: special == SpecialKey::Backspace,
                    })),
                },
            )
            .unwrap();
        let response = reply(response);
        if let Some(commit) = &response.commit {
            self.document.push_str(&commit.value);
        }
        self.revision = response.revision.unwrap();
        self.engine.handle(self.id, ack(&response, true)).unwrap();
        if special == SpecialKey::Backspace {
            self.document.pop();
        }
        self.last = response;
    }
    fn convert(&mut self) {
        for ch in "kousei".chars() {
            self.key(&ch.to_string(), SpecialKey::None);
        }
        self.key("", SpecialKey::Space);
    }
    fn choose(&mut self, value: &str) -> KeyReply {
        self.seq += 1;
        let item = self
            .last
            .candidates
            .as_ref()
            .unwrap()
            .items
            .iter()
            .find(|c| c.value == value)
            .unwrap();
        reply(
            self.engine
                .handle(
                    self.id,
                    Request {
                        body: Some(request::Body::Select(SelectRequest {
                            revision: Some(self.revision),
                            event_seq: self.seq,
                            handle: item.handle,
                            submit: true,
                        })),
                    },
                )
                .unwrap(),
        )
    }
    fn confirm(&mut self, response: KeyReply) {
        self.document
            .push_str(&response.commit.as_ref().unwrap().value);
        self.revision = response.revision.unwrap();
        self.engine.handle(self.id, ack(&response, true)).unwrap();
        self.last = response;
    }
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn standard_character_conversions_preserve_composition_and_commit() {
    for convert in [None, Some(SpecialKey::Space), Some(SpecialKey::Tab)] {
        for (control, expected) in [
            ("u", "にほんご"),
            ("i", "ニホンゴ"),
            ("o", "ﾆﾎﾝｺﾞ"),
            ("p", "ｎｉｈｏｎｇｏ"),
            ("t", "nihongo"),
        ] {
            let (_temp, engine, id, revision) = engine();
            let mut editor = Editor {
                engine,
                id,
                revision,
                seq: 0,
                document: String::new(),
                last: Default::default(),
            };
            for ch in "nihongo".chars() {
                editor.key(&ch.to_string(), SpecialKey::None);
            }
            if let Some(convert) = convert {
                editor.key("", convert);
            }
            editor.modified_key(control, SpecialKey::None, MOD_CONTROL);
            assert!(editor.last.consumed);
            assert_eq!(surface(editor.last.preedit.as_ref().unwrap()), expected);
            assert!(editor.document.is_empty());
            editor.key("", SpecialKey::Enter);
            assert_eq!(editor.document, expected);
        }
    }
    let (_temp, engine, id, revision) = engine();
    let mut editor = Editor {
        engine,
        id,
        revision,
        seq: 0,
        document: String::new(),
        last: Default::default(),
    };
    for control in ["u", "i", "o", "p", "t"] {
        editor.modified_key(control, SpecialKey::None, MOD_CONTROL);
        assert!(!editor.last.consumed);
        assert!(editor.last.commit.is_none());
        assert!(editor.document.is_empty());
    }
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn alphanumeric_case_cycling_is_not_repeated_by_request_retries() {
    let (_temp, mut engine, id, mut revision) = engine();
    let mut seq = 0;
    for ch in "kanade".chars() {
        seq += 1;
        let response = reply(
            engine
                .handle(id, input(revision, seq, &ch.to_string()))
                .unwrap(),
        );
        revision = response.revision.unwrap();
        engine.handle(id, ack(&response, true)).unwrap();
    }
    for expected in ["kanade", "KANADE", "Kanade", "kanade"] {
        seq += 1;
        let mut request = input(revision, seq, "t");
        if let Some(request::Body::Key(k)) = &mut request.body {
            k.key.as_mut().unwrap().modifiers = MOD_CONTROL;
        }
        let response = reply(engine.handle(id, request.clone()).unwrap());
        assert_eq!(response, reply(engine.handle(id, request).unwrap()));
        assert_eq!(surface(response.preedit.as_ref().unwrap()), expected);
        revision = response.revision.unwrap();
        engine.handle(id, ack(&response, true)).unwrap();
    }
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn character_conversion_handles_pending_romaji_and_halfwidth_dakuten() {
    for (typed, control, expected) in [
        ("n", "t", "n"),
        ("k", "t", "k"),
        ("tt", "t", "tt"),
        ("kanade123", "t", "kanade123"),
        ("kanade123", "p", "ｋａｎａｄｅ１２３"),
        ("gakkou", "o", "ｶﾞｯｺｳ"),
    ] {
        let (_temp, engine, id, revision) = engine();
        let mut editor = Editor {
            engine,
            id,
            revision,
            seq: 0,
            document: String::new(),
            last: Default::default(),
        };
        for ch in typed.chars() {
            editor.key(&ch.to_string(), SpecialKey::None);
        }
        editor.modified_key(control, SpecialKey::None, MOD_CONTROL);
        let preedit = editor.last.preedit.as_ref().unwrap();
        assert_eq!(surface(preedit), expected);
        assert_eq!(preedit.cursor_utf16, utf16_len(expected));
        assert_eq!(
            preedit.segments.iter().map(|s| s.value_utf16).sum::<u32>(),
            utf16_len(expected)
        );
        editor.key("", SpecialKey::Enter);
        assert_eq!(editor.document, expected);
    }
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn character_conversion_is_not_replaced_by_learned_kanji() {
    use kanade_learn::{Correction, Evidence, Store, service::LearningService};
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("learning.json");
    let mut store = Store::default();
    store
        .apply(
            Correction {
                reading: "こうせい".into(),
                accepted: "校正".into(),
                rejected: Some("構成".into()),
                evidence: Evidence::Strong,
            },
            1,
            false,
        )
        .unwrap();
    store.save(&path).unwrap();
    // Morphology is lazy and is not needed to read this existing preference.
    let learner = LearningService::start(path, temp.path().join("unused.hsd")).unwrap();
    let backend = Mozc::start(&kanade_mozc::default_worker(), &temp.path().join("mozc")).unwrap();
    let mut engine = Engine::new(backend, Some(learner));
    let welcome = engine
        .open(&Hello {
            min_version: 2,
            max_version: 2,
        })
        .unwrap();
    let mut editor = Editor {
        engine,
        id: welcome.connection,
        revision: Revision {
            connection: welcome.connection,
            context: 1,
            composition: 0,
        },
        seq: 0,
        document: String::new(),
        last: Default::default(),
    };
    editor.convert();
    assert_eq!(surface(editor.last.preedit.as_ref().unwrap()), "校正");
    editor.key("", SpecialKey::Escape);
    editor.key("", SpecialKey::Escape);
    for (control, expected) in [
        ("u", "こうせい"),
        ("i", "コウセイ"),
        ("o", "ｺｳｾｲ"),
        ("p", "ｋｏｕｓｅｉ"),
        ("t", "kousei"),
    ] {
        for ch in "kousei".chars() {
            editor.key(&ch.to_string(), SpecialKey::None);
        }
        editor.modified_key(control, SpecialKey::None, MOD_CONTROL);
        assert_eq!(surface(editor.last.preedit.as_ref().unwrap()), expected);
        editor.key("", SpecialKey::Enter);
    }
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn ordinary_candidate_digits_match_clicks_on_each_page() {
    for click in [true, false] {
        for down in [0, 9] {
            let (_temp, engine, id, revision) = engine();
            let mut editor = Editor {
                engine,
                id,
                revision,
                seq: 0,
                document: String::new(),
                last: Default::default(),
            };
            editor.convert();
            for _ in 0..down {
                editor.key("", SpecialKey::Down);
            }
            let page = editor.last.candidates.as_ref().unwrap();
            assert_eq!(page.start, down);
            let item = page.items.iter().find(|item| item.number == 2).unwrap();
            let expected = item.value.clone();
            if click {
                let response = editor.choose(&expected);
                editor.confirm(response);
            } else {
                editor.key("2", SpecialKey::None);
            }
            assert_eq!(editor.document, expected);
            assert!(
                editor
                    .last
                    .preedit
                    .as_ref()
                    .is_none_or(|p| p.segments.is_empty())
            );
        }
    }
}

#[test]
#[ignore = "requires the locked worker and KANADE_TEST_DICT; mise run learning-contract"]
fn dictionary_correction_persists_after_ack_and_is_revoked_after_deletion() {
    use kanade_learn::service::LearningService;
    let temp = tempfile::tempdir().unwrap();
    let dict = std::env::var_os("KANADE_TEST_DICT")
        .expect("KANADE_TEST_DICT must point to the pinned IPAdic dictionary");
    let learner =
        LearningService::start(temp.path().join("kanade/learning.json"), dict.into()).unwrap();
    let backend = Mozc::start(&kanade_mozc::default_worker(), &temp.path().join("mozc")).unwrap();
    let mut engine = Engine::new(backend, Some(learner.clone()));
    let welcome = engine
        .open(&Hello {
            min_version: 2,
            max_version: 2,
        })
        .unwrap();
    let mut editor = Editor {
        engine,
        id: welcome.connection,
        revision: Revision {
            connection: welcome.connection,
            context: 1,
            composition: 0,
        },
        seq: 0,
        document: String::new(),
        last: Default::default(),
    };
    editor.convert();
    let original = editor.choose("構成");
    editor.confirm(original);
    for _ in 0..2 {
        editor.key("", SpecialKey::Backspace);
    }
    editor.convert();
    let correction = editor.choose("校正");
    learner.flush().unwrap();
    assert_eq!(learner.read(|s| s.records().count()), Some(0));
    editor.confirm(correction);
    learner.flush().unwrap();
    let records = learner
        .read(|s| s.records().cloned().collect::<Vec<_>>())
        .unwrap();
    assert_eq!(records.len(), 2);
    let correction = records
        .iter()
        .find(|record| record.selection_count == 0)
        .unwrap();
    let selection = records
        .iter()
        .find(|record| record.selection_count > 0)
        .unwrap();
    assert_eq!(correction.correction.accepted, "校正");
    assert_eq!(correction.correction.rejected.as_deref(), Some("構成"));
    assert_eq!(selection.correction.accepted, "校正");
    assert_eq!(selection.selection_count, 1);
    assert!(!selection.vocabulary);
    editor.key("", SpecialKey::Backspace);
    editor.key("", SpecialKey::Backspace);
    editor.key("a", SpecialKey::None);
    learner.flush().unwrap();
    assert_eq!(learner.read(|s| s.records().count()), Some(0));
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn candidate_pages_keep_script_forms_without_duplicate_surfaces() {
    let (_temp, engine, id, revision) = engine();
    let mut editor = Editor {
        engine,
        id,
        revision,
        seq: 0,
        document: String::new(),
        last: Default::default(),
    };
    for ch in "hyoujisareru".chars() {
        editor.key(&ch.to_string(), SpecialKey::None);
    }
    editor.key("", SpecialKey::Space);
    let total = editor.last.candidates.as_ref().unwrap().total;
    assert!(total > 0 && total < 1000);
    let mut slots = std::collections::BTreeMap::new();
    for _ in 0..total {
        let page = editor.last.candidates.as_ref().unwrap();
        assert_eq!(page.total, total);
        for item in &page.items {
            slots.insert(item.handle.unwrap().slot, item.value.clone());
        }
        editor.key("", SpecialKey::Down);
    }
    assert_eq!(slots.len(), total as usize);
    assert_eq!(
        slots
            .values()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        slots.len()
    );
    assert!(slots.values().any(|v| v == "ひょうじされる"));
    assert!(slots.values().any(|v| v == "ヒョウジサレル"));
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn candidate_history_deletion_is_scoped_cached_and_does_not_replay_commits() {
    use kanade_mozc::ConversionEngine;
    let profile = tempfile::tempdir().unwrap();
    let mut backend = Mozc::start(&kanade_mozc::default_worker(), profile.path()).unwrap();
    backend.set_privacy(Privacy::Normal).unwrap();
    let session = backend.create_session().unwrap();
    let convert = |backend: &mut Mozc| {
        for ch in "kami.".chars() {
            backend
                .key(
                    session,
                    &Key {
                        text: ch.to_string(),
                        ..Default::default()
                    },
                    "",
                )
                .unwrap();
        }
        backend
            .key(
                session,
                &Key {
                    special: SpecialKey::Space as i32,
                    ..Default::default()
                },
                "",
            )
            .unwrap()
    };
    let initial = convert(&mut backend);
    let baseline = initial.preedit.segments[0].value.clone();
    let learned = initial
        .candidates
        .iter()
        .find(|c| !c.transliteration && c.value != baseline)
        .unwrap()
        .value
        .clone();
    backend.reset(session).unwrap();
    for _ in 0..4 {
        convert(&mut backend);
        backend.select(session, &learned).unwrap();
        backend.submit(session).unwrap();
    }
    backend.delete_session(session).unwrap();
    let mut engine = Engine::new(backend, None);
    let welcome = engine
        .open(&Hello {
            min_version: 2,
            max_version: 2,
        })
        .unwrap();
    let mut editor = Editor {
        engine,
        id: welcome.connection,
        revision: Revision {
            connection: welcome.connection,
            context: 1,
            composition: 0,
        },
        seq: 0,
        document: String::new(),
        last: KeyReply::default(),
    };
    for ch in "kami.".chars() {
        editor.key(&ch.to_string(), SpecialKey::None);
    }
    editor.key("", SpecialKey::Space);
    assert_eq!(
        editor.last.preedit.as_ref().unwrap().segments[0].value,
        learned
    );
    editor.engine.configure(kanade_engine::config::Config {
        punctuation_commit: true,
        ..Default::default()
    });
    for privacy in [Privacy::Private, Privacy::Unknown] {
        editor.seq += 1;
        let before = editor.last.preedit.clone();
        let request = Request {
            body: Some(request::Body::Key(KeyRequest {
                revision: Some(editor.revision),
                event_seq: editor.seq,
                key: Some(Key {
                    special: SpecialKey::Delete as i32,
                    modifiers: MOD_CONTROL,
                    ..Default::default()
                }),
                context: Some(Context {
                    privacy: privacy as i32,
                    app: "com.apple.TextEdit".into(),
                    ..Default::default()
                }),
                passed_through: false,
            })),
        };
        let response = reply(editor.engine.handle(editor.id, request).unwrap());
        assert_eq!(response.preedit, before);
        assert!(response.commit.is_none());
        editor
            .engine
            .handle(editor.id, ack(&response, true))
            .unwrap();
        editor.revision = response.revision.unwrap();
        editor.last = response;
    }
    editor.seq += 1;
    let deletion = Request {
        body: Some(request::Body::Key(KeyRequest {
            revision: Some(editor.revision),
            event_seq: editor.seq,
            key: Some(Key {
                special: SpecialKey::Delete as i32,
                modifiers: MOD_CONTROL,
                ..Default::default()
            }),
            context: Some(Context {
                privacy: Privacy::Normal as i32,
                app: "com.apple.TextEdit".into(),
                ..Default::default()
            }),
            passed_through: false,
        })),
    };
    let deleted = reply(editor.engine.handle(editor.id, deletion.clone()).unwrap());
    assert!(deleted.commit.is_none());
    assert_eq!(reading(deleted.preedit.as_ref().unwrap()), "かみ。");
    assert!(deleted.candidates.is_none());
    assert_eq!(
        reply(editor.engine.handle(editor.id, deletion.clone()).unwrap()),
        deleted
    );
    editor
        .engine
        .handle(editor.id, ack(&deleted, true))
        .unwrap();
    assert_eq!(
        reply(editor.engine.handle(editor.id, deletion).unwrap()),
        deleted
    );
    editor.revision = deleted.revision.unwrap();
    editor.last = deleted;
    editor.key("", SpecialKey::Space);
    assert_eq!(
        editor.last.preedit.as_ref().unwrap().segments[0].value,
        baseline
    );
    editor.modified_key("t", SpecialKey::None, MOD_CONTROL);
    let script = editor.last.preedit.clone();
    editor.modified_key("", SpecialKey::Delete, MOD_CONTROL);
    assert_eq!(editor.last.preedit, script);
    assert!(editor.last.commit.is_none());
    editor.key("", SpecialKey::Enter);
    let document = editor.document.clone();
    let committed = editor.last.clone();
    editor.seq += 1;
    let passed = reply(
        editor
            .engine
            .handle(
                editor.id,
                Request {
                    body: Some(request::Body::Key(KeyRequest {
                        revision: Some(editor.revision),
                        event_seq: editor.seq,
                        key: Some(Key {
                            special: SpecialKey::Delete as i32,
                            modifiers: MOD_CONTROL,
                            ..Default::default()
                        }),
                        context: Some(Context {
                            privacy: Privacy::Normal as i32,
                            app: "com.apple.TextEdit".into(),
                            ..Default::default()
                        }),
                        passed_through: true,
                    })),
                },
            )
            .unwrap(),
    );
    assert!(!passed.consumed);
    assert!(passed.commit.is_none());
    assert!(committed.commit.is_some());
    assert_eq!(editor.document, document);
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn calculated_dates_replace_all_segments_and_keep_the_displayed_date_across_midnight() {
    use kanade_engine::dates::Date;
    for click in [true, false] {
        let (_profile, mut engine, id, revision) = engine();
        engine.set_date(Date::new(2024, 1, 31));
        let mut editor = Editor {
            engine,
            id,
            revision,
            seq: 0,
            document: String::new(),
            last: KeyReply::default(),
        };
        for ch in "raigetsumatsu".chars() {
            editor.key(&ch.to_string(), SpecialKey::None);
        }
        editor.key("", SpecialKey::Space);
        editor.modified_key("", SpecialKey::Left, MOD_SHIFT);
        assert!(editor.last.preedit.as_ref().unwrap().segments.len() > 1);
        if !click {
            editor.key("", SpecialKey::Right);
        }
        let candidate = editor
            .last
            .candidates
            .as_ref()
            .unwrap()
            .items
            .iter()
            .find(|c| c.source == CandidateSource::Date as i32 && c.value == "2024-02-29")
            .unwrap()
            .clone();
        editor.engine.set_date(Date::new(2024, 2, 1));
        editor.seq += 1;
        let preview = reply(
            editor
                .engine
                .handle(
                    id,
                    Request {
                        body: Some(request::Body::Select(SelectRequest {
                            revision: Some(editor.revision),
                            event_seq: editor.seq,
                            handle: candidate.handle,
                            submit: false,
                        })),
                    },
                )
                .unwrap(),
        );
        assert!(preview.commit.is_none());
        assert_eq!(surface(preview.preedit.as_ref().unwrap()), "2024-02-29");
        assert_eq!(reading(preview.preedit.as_ref().unwrap()), "らいげつまつ");
        editor.engine.handle(id, ack(&preview, true)).unwrap();
        editor.revision = preview.revision.unwrap();
        editor.last = preview;
        let page = editor.last.candidates.as_ref().unwrap();
        let candidate = page
            .items
            .iter()
            .find(|c| c.source == CandidateSource::Date as i32 && c.value == "2024-02-29")
            .unwrap();
        assert_eq!(page.selected, candidate.handle.unwrap().slot);
        let handle = candidate.handle;
        let number = candidate.number;
        editor.seq += 1;
        let request = if click {
            Request {
                body: Some(request::Body::Select(SelectRequest {
                    revision: Some(editor.revision),
                    event_seq: editor.seq,
                    handle,
                    submit: true,
                })),
            }
        } else {
            Request {
                body: Some(request::Body::Key(KeyRequest {
                    revision: Some(editor.revision),
                    event_seq: editor.seq,
                    key: Some(Key {
                        text: number.to_string(),
                        ..Default::default()
                    }),
                    context: Some(Context {
                        privacy: Privacy::Normal as i32,
                        app: "com.apple.TextEdit".into(),
                        ..Default::default()
                    }),
                    passed_through: false,
                })),
            }
        };
        let committed = reply(editor.engine.handle(id, request.clone()).unwrap());
        let commit = committed.commit.as_ref().unwrap();
        assert_eq!(commit.value, "2024-02-29");
        assert_eq!(commit.reading, "らいげつまつ");
        assert!(commit.synthetic && commit.transient);
        assert_eq!(reply(editor.engine.handle(id, request).unwrap()), committed);
        editor.confirm(committed);
        assert_eq!(editor.document, "2024-02-29");
        // A new composition sees the refreshed day; the previous one was frozen.
        for ch in "raigetsumatsu".chars() {
            editor.key(&ch.to_string(), SpecialKey::None);
        }
        editor.key("", SpecialKey::Space);
        let page = editor.last.candidates.as_ref().unwrap();
        let candidate = page
            .items
            .iter()
            .find(|c| c.source == CandidateSource::Date as i32 && c.value == "2024-03-31")
            .unwrap()
            .clone();
        editor.engine.configure(kanade_engine::config::Config {
            calculated_dates: false,
            ..Default::default()
        });
        editor.seq += 1;
        assert_eq!(
            editor.engine.handle(
                id,
                Request {
                    body: Some(request::Body::Select(SelectRequest {
                        revision: Some(editor.revision),
                        event_seq: editor.seq,
                        handle: candidate.handle,
                        submit: true,
                    }))
                }
            ),
            Err(failure::Code::Stale)
        );
    }
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn calculated_dates_are_not_added_to_persistent_mozc_history() {
    use kanade_engine::dates::Date;
    let (_profile, mut engine, id, revision) = engine();
    engine.set_date(Date::new(2024, 1, 31));
    let mut editor = Editor {
        engine,
        id,
        revision,
        seq: 0,
        document: String::new(),
        last: KeyReply::default(),
    };
    for ch in "raigetsumatsu".chars() {
        editor.key(&ch.to_string(), SpecialKey::None);
    }
    editor.key("", SpecialKey::Space);
    let committed = editor.choose("2024-02-29");
    assert!(committed.commit.as_ref().unwrap().transient);
    editor.confirm(committed);
    // This is the point where an ordinary synthetic word would enter Mozc history.
    editor.key("a", SpecialKey::None);
    editor.key("", SpecialKey::Escape);
    editor.engine.sync().unwrap();
    editor.engine.recover_worker().unwrap();
    editor.engine.configure(kanade_engine::config::Config {
        calculated_dates: false,
        ..Default::default()
    });
    let welcome = editor
        .engine
        .open(&Hello {
            min_version: 2,
            max_version: 2,
        })
        .unwrap();
    editor.id = welcome.connection;
    editor.revision = Revision {
        connection: welcome.connection,
        context: 1,
        composition: 0,
    };
    editor.seq = 0;
    editor.document.clear();
    for ch in "raigetsumatsu".chars() {
        editor.key(&ch.to_string(), SpecialKey::None);
    }
    editor.key("", SpecialKey::Tab);
    assert!(
        editor
            .last
            .candidates
            .as_ref()
            .is_none_or(|page| page.items.iter().all(|c| c.value != "2024-02-29"))
    );
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn typing_after_a_date_preview_commits_the_displayed_value_once() {
    use kanade_engine::dates::Date;
    for text in ["a", "A", "0", "."] {
        let (_profile, mut engine, id, revision) = engine();
        engine.set_date(Date::new(2024, 1, 31));
        let mut editor = Editor {
            engine,
            id,
            revision,
            seq: 0,
            document: String::new(),
            last: KeyReply::default(),
        };
        for ch in "raigetsumatsu".chars() {
            editor.key(&ch.to_string(), SpecialKey::None);
        }
        editor.key("", SpecialKey::Space);
        let values: Vec<_> = editor
            .last
            .candidates
            .as_ref()
            .unwrap()
            .items
            .iter()
            .map(|c| c.value.clone())
            .collect();
        editor.key("", SpecialKey::Down);
        assert_eq!(surface(editor.last.preedit.as_ref().unwrap()), "2024-02-29");
        assert!(
            editor
                .last
                .candidates
                .as_ref()
                .unwrap()
                .items
                .iter()
                .map(|c| c.value.clone())
                .collect::<Vec<_>>()
                == values
        );
        editor.key("", SpecialKey::Up);
        editor.key("", SpecialKey::Down);
        assert_eq!(surface(editor.last.preedit.as_ref().unwrap()), "2024-02-29");
        assert!(
            editor
                .last
                .candidates
                .as_ref()
                .unwrap()
                .items
                .iter()
                .map(|c| c.value.clone())
                .collect::<Vec<_>>()
                == values
        );
        editor.engine.configure(kanade_engine::config::Config {
            punctuation_commit: true,
            ..Default::default()
        });
        editor.seq += 1;
        let request = Request {
            body: Some(request::Body::Key(KeyRequest {
                revision: Some(editor.revision),
                event_seq: editor.seq,
                key: Some(Key {
                    text: text.into(),
                    modifiers: if text == "A" { MOD_SHIFT } else { 0 },
                    ..Default::default()
                }),
                context: Some(Context {
                    privacy: Privacy::Normal as i32,
                    app: "com.apple.TextEdit".into(),
                    ..Default::default()
                }),
                passed_through: false,
            })),
        };
        let result = reply(editor.engine.handle(id, request.clone()).unwrap());
        assert!(result.commit.as_ref().unwrap().transient);
        assert_eq!(reply(editor.engine.handle(id, request).unwrap()), result);
        let consumed = result.consumed;
        editor.confirm(result);
        if text == "." {
            assert_eq!(editor.document, "2024-02-29。");
        } else if text == "A" {
            assert!(!consumed);
            // The native host inserts the unconsumed shifted letter after applying
            // the explicit prefix commit, exactly as for ordinary early commits.
            editor.document.push_str(text);
            assert_eq!(editor.document, "2024-02-29A");
            assert!(editor.last.preedit.as_ref().unwrap().segments.is_empty());
        } else {
            assert_eq!(editor.document, "2024-02-29");
            let next = surface(editor.last.preedit.as_ref().unwrap());
            assert!(!next.is_empty());
            editor.key("", SpecialKey::Enter);
            assert_eq!(editor.document, format!("2024-02-29{next}"));
        }
    }
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn a_number_without_a_row_on_the_last_candidate_page_does_not_commit() {
    let (_profile, engine, id, revision) = engine();
    let mut editor = Editor {
        engine,
        id,
        revision,
        seq: 0,
        document: String::new(),
        last: KeyReply::default(),
    };
    for ch in "kana".chars() {
        editor.key(&ch.to_string(), SpecialKey::None);
    }
    editor.key("", SpecialKey::Space);
    let total = editor.last.candidates.as_ref().unwrap().total;
    let page_size = (2..=9)
        .rev()
        .find(|size| !total.is_multiple_of(*size))
        .unwrap();
    editor.engine.configure(kanade_engine::config::Config {
        page_size,
        ..Default::default()
    });
    while editor.last.candidates.as_ref().unwrap().selected != total - 1 {
        editor.key("", SpecialKey::Down);
    }
    let page = editor.last.candidates.as_ref().unwrap();
    let invalid = (page.items.len() + 1).to_string();
    let preedit = editor.last.preedit.clone();
    let selected = page.selected;
    editor.key(&invalid, SpecialKey::None);
    assert!(editor.last.consumed);
    assert!(editor.last.commit.is_none());
    assert_eq!(editor.last.preedit, preedit);
    assert_eq!(editor.last.candidates.as_ref().unwrap().selected, selected);
    assert!(editor.document.is_empty());
}

fn emoji_editor(roman: &str) -> (tempfile::TempDir, Editor) {
    let (profile, engine, id, revision) = engine();
    let mut editor = Editor {
        engine,
        id,
        revision,
        seq: 0,
        document: String::new(),
        last: KeyReply::default(),
    };
    for ch in roman.chars() {
        editor.key(&ch.to_string(), SpecialKey::None);
    }
    editor.key("", SpecialKey::Space);
    if roman == "watashiharoketto" {
        let count = editor.last.preedit.as_ref().unwrap().segments.len();
        for _ in 1..count {
            editor.key("", SpecialKey::Right);
        }
    }
    (profile, editor)
}

fn emoji_item(editor: &mut Editor, value: &str) -> CandidateItem {
    for _ in 0..1024 {
        let page = editor.last.candidates.as_ref().unwrap();
        if let Some(item) = page
            .items
            .iter()
            .find(|c| c.source == CandidateSource::Emoji as i32 && c.value == value)
        {
            return item.clone();
        }
        editor.key("", SpecialKey::Down);
    }
    panic!("emoji fixture candidate missing: {value}");
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn emoji_clicks_digits_scopes_and_utf16_agree() {
    for (roman, value, prefix) in [
        ("iine", "👍", ""),
        ("akaiha-to", "❤️", ""),
        ("watashiharoketto", "🚀", "私は"),
    ] {
        for click in [true, false] {
            let (_profile, mut editor) = emoji_editor(roman);
            let item = emoji_item(&mut editor, value);
            let page = editor.last.candidates.as_ref().unwrap();
            let mut identities = std::collections::BTreeSet::new();
            assert!(
                page.items
                    .iter()
                    .all(|c| identities.insert(kanade_emoji::identity(&c.value)))
            );
            editor.seq += 1;
            let body = if click {
                request::Body::Select(SelectRequest {
                    revision: Some(editor.revision),
                    event_seq: editor.seq,
                    handle: item.handle,
                    submit: true,
                })
            } else {
                request::Body::Key(KeyRequest {
                    revision: Some(editor.revision),
                    event_seq: editor.seq,
                    key: Some(Key {
                        text: item.number.to_string(),
                        ..Default::default()
                    }),
                    context: Some(Context {
                        privacy: Privacy::Normal as i32,
                        app: "com.apple.TextEdit".into(),
                        ..Default::default()
                    }),
                    passed_through: false,
                })
            };
            let request = Request { body: Some(body) };
            let result = reply(editor.engine.handle(editor.id, request.clone()).unwrap());
            assert_eq!(
                reply(editor.engine.handle(editor.id, request).unwrap()),
                result
            );
            let commit = result.commit.as_ref().unwrap();
            assert!(commit.synthetic && commit.transient);
            assert_eq!(commit.value, format!("{prefix}{value}"));
            editor.confirm(result);
            assert_eq!(
                editor.document.encode_utf16().count(),
                prefix.encode_utf16().count() + value.encode_utf16().count()
            );
        }
    }
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn emoji_preview_can_move_segments_continue_and_reject_disabled_handles() {
    let (_profile, mut editor) = emoji_editor("watashiharoketto");
    let item = emoji_item(&mut editor, "🚀");
    editor.seq += 1;
    let preview = reply(
        editor
            .engine
            .handle(
                editor.id,
                Request {
                    body: Some(request::Body::Select(SelectRequest {
                        revision: Some(editor.revision),
                        event_seq: editor.seq,
                        handle: item.handle,
                        submit: false,
                    })),
                },
            )
            .unwrap(),
    );
    assert_eq!(surface(preview.preedit.as_ref().unwrap()), "私は🚀");
    assert_eq!(
        preview.preedit.as_ref().unwrap().cursor_utf16,
        utf16_len("私は🚀")
    );
    editor
        .engine
        .handle(editor.id, ack(&preview, true))
        .unwrap();
    editor.revision = preview.revision.unwrap();
    editor.last = preview;
    editor.key("", SpecialKey::Left);
    editor.key("", SpecialKey::Right);
    let item = emoji_item(&mut editor, "🚀");
    editor.seq += 1;
    let preview = reply(
        editor
            .engine
            .handle(
                editor.id,
                Request {
                    body: Some(request::Body::Select(SelectRequest {
                        revision: Some(editor.revision),
                        event_seq: editor.seq,
                        handle: item.handle,
                        submit: false,
                    })),
                },
            )
            .unwrap(),
    );
    editor
        .engine
        .handle(editor.id, ack(&preview, true))
        .unwrap();
    editor.revision = preview.revision.unwrap();
    editor.last = preview;
    editor.key("0", SpecialKey::None);
    assert_eq!(editor.document, "私は🚀");
    assert!(editor.last.commit.as_ref().unwrap().transient);

    let (_profile, mut editor) = emoji_editor("iine");
    let item = emoji_item(&mut editor, "👍");
    editor.engine.configure(kanade_engine::config::Config {
        emoji_dictionary: false,
        ..Default::default()
    });
    editor.seq += 1;
    let response = editor.engine.handle(
        editor.id,
        Request {
            body: Some(request::Body::Select(SelectRequest {
                revision: Some(editor.revision),
                event_seq: editor.seq,
                handle: item.handle,
                submit: true,
            })),
        },
    );
    assert_eq!(response, Err(failure::Code::Stale));
    assert!(editor.document.is_empty());
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn emoji_transient_commits_do_not_create_prediction_history() {
    let (_profile, mut editor) = emoji_editor("uchuuryokou");
    let _ = emoji_item(&mut editor, "🚀");
    let committed = editor.choose("🚀");
    assert!(committed.commit.as_ref().unwrap().transient);
    editor.confirm(committed);
    editor.key("a", SpecialKey::None);
    editor.key("", SpecialKey::Escape);
    editor.engine.sync().unwrap();
    editor.engine.recover_worker().unwrap();
    editor.engine.configure(kanade_engine::config::Config {
        emoji_dictionary: false,
        ..Default::default()
    });
    let welcome = editor
        .engine
        .open(&Hello {
            min_version: 2,
            max_version: 2,
        })
        .unwrap();
    editor.id = welcome.connection;
    editor.revision = Revision {
        connection: welcome.connection,
        context: 1,
        composition: 0,
    };
    editor.seq = 0;
    editor.document.clear();
    for ch in "uchuuryokou".chars() {
        editor.key(&ch.to_string(), SpecialKey::None);
    }
    editor.key("", SpecialKey::Tab);
    assert!(
        editor
            .last
            .candidates
            .as_ref()
            .is_none_or(|page| page.items.iter().all(|c| c.value != "🚀"))
    );
}
