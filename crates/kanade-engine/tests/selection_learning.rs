//! Candidate-choice learning must be independent of the worker's own history.
use kanade_engine::Engine;
use kanade_ipc::*;
use kanade_learn::service::LearningService;
use kanade_mozc::Mozc;

struct Editor {
    engine: Engine<Mozc>,
    id: u64,
    revision: Revision,
    seq: u64,
    last: KeyReply,
    privacy: Privacy,
    app: String,
}
impl Editor {
    fn new(profile: &std::path::Path, learning: LearningService) -> Self {
        let backend = Mozc::start(&kanade_mozc::default_worker(), profile).unwrap();
        let mut engine = Engine::new(backend, Some(learning));
        let welcome = engine
            .open(&Hello {
                min_version: 2,
                max_version: 2,
            })
            .unwrap();
        Self {
            engine,
            id: welcome.connection,
            revision: Revision {
                connection: welcome.connection,
                context: 1,
                composition: 0,
            },
            seq: 0,
            last: KeyReply::default(),
            privacy: Privacy::Normal,
            app: "com.apple.TextEdit".into(),
        }
    }
    fn response(&mut self, request: Request) -> KeyReply {
        let response = self.engine.handle(self.id, request).unwrap().unwrap();
        let Some(response::Body::Key(reply)) = response.body else {
            panic!("key reply required")
        };
        self.revision = reply.revision.unwrap();
        self.last = reply.clone();
        reply
    }
    fn acknowledge(&mut self, applied: bool) {
        self.engine
            .handle(
                self.id,
                Request {
                    body: Some(request::Body::Ack(Ack {
                        revision: self.last.revision,
                        event_seq: self.last.event_seq,
                        applied,
                    })),
                },
            )
            .unwrap();
    }
    fn key(&mut self, text: &str, special: SpecialKey) {
        self.seq += 1;
        self.response(Request {
            body: Some(request::Body::Key(KeyRequest {
                revision: Some(self.revision),
                event_seq: self.seq,
                key: Some(Key {
                    text: text.into(),
                    special: special as i32,
                    ..Default::default()
                }),
                context: Some(Context {
                    privacy: self.privacy as i32,
                    app: self.app.clone(),
                    ..Default::default()
                }),
                passed_through: false,
            })),
        });
        self.acknowledge(true);
    }
    fn convert(&mut self) {
        for ch in "kousei".chars() {
            self.key(&ch.to_string(), SpecialKey::None);
        }
        self.key("", SpecialKey::Space);
    }
    fn choose_third(&mut self) -> String {
        let item = self.last.candidates.as_ref().unwrap().items[2].clone();
        self.seq += 1;
        self.response(Request {
            body: Some(request::Body::Select(SelectRequest {
                revision: Some(self.revision),
                event_seq: self.seq,
                handle: item.handle,
                submit: true,
            })),
        });
        assert!(
            self.last
                .commit
                .as_ref()
                .unwrap()
                .words
                .iter()
                .any(|word| word.reading == item.reading && word.value == item.value)
        );
        item.value
    }
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn default_learning_works_across_two_non_textedit_apps() {
    let temp = tempfile::tempdir().unwrap();
    let learning = LearningService::start(
        temp.path().join("learning.json"),
        temp.path().join("absent.hsd"),
    )
    .unwrap();
    let mut editor = Editor::new(&temp.path().join("first"), learning.clone());
    editor.app = "com.example.FirstEditor".into();
    editor.convert();
    let chosen = editor.choose_third();
    editor.acknowledge(true);
    learning.flush().unwrap();
    assert_eq!(learning.read(|s| s.records().count()), Some(1));
    drop(editor);
    let mut editor = Editor::new(&temp.path().join("second"), learning);
    editor.app = "com.example.SecondEditor".into();
    editor.convert();
    assert_eq!(
        editor.last.candidates.as_ref().unwrap().items[0].value,
        chosen
    );
    assert_eq!(editor.last.candidates.as_ref().unwrap().selected, 0);
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn exclusions_stop_a_stale_client_and_allow_learning_again_after_removal() {
    let temp = tempfile::tempdir().unwrap();
    let learning = LearningService::start(
        temp.path().join("learning.json"),
        temp.path().join("absent.hsd"),
    )
    .unwrap();
    let mut editor = Editor::new(&temp.path().join("worker"), learning.clone());
    editor.app = "com.example.Editor".into();
    editor.convert();
    editor.choose_third();
    editor.engine.configure(kanade_engine::config::Config {
        learning_excluded_apps: vec![editor.app.clone()],
        ..Default::default()
    });
    editor.acknowledge(true);
    learning.flush().unwrap();
    assert_eq!(learning.read(|s| s.records().count()), Some(0));
    // The frontend is still claiming Normal on the original connection.
    editor.convert();
    editor.choose_third();
    editor.acknowledge(true);
    learning.flush().unwrap();
    assert_eq!(learning.read(|s| s.records().count()), Some(0));
    editor.engine.configure(Default::default());
    editor.convert();
    editor.choose_third();
    editor.acknowledge(true);
    learning.flush().unwrap();
    assert_eq!(learning.read(|s| s.records().count()), Some(1));
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn third_choice_is_acknowledged_once_and_becomes_first_with_a_fresh_worker() {
    let temp = tempfile::tempdir().unwrap();
    let learning = LearningService::start(
        temp.path().join("learning.json"),
        temp.path().join("absent.hsd"),
    )
    .unwrap();
    let mut editor = Editor::new(&temp.path().join("first-worker"), learning.clone());
    editor.convert();
    let chosen = editor.choose_third();
    learning.flush().unwrap();
    assert_eq!(learning.read(|s| s.records().count()), Some(0));
    editor.acknowledge(true);
    editor.acknowledge(true);
    learning.flush().unwrap();
    assert_eq!(learning.read(|s| s.records().count()), Some(1));
    drop(editor);
    drop(learning);

    let learning = LearningService::start(
        temp.path().join("learning.json"),
        temp.path().join("absent.hsd"),
    )
    .unwrap();
    let mut editor = Editor::new(&temp.path().join("fresh-worker"), learning.clone());
    editor.convert();
    let page = editor.last.candidates.as_ref().unwrap();
    assert_eq!(page.items[0].value, chosen);
    assert_eq!(page.selected, 0);
    assert_eq!(surface(editor.last.preedit.as_ref().unwrap()), chosen);
    // Accepting the default must not reinforce an automatic preference.
    editor.key("", SpecialKey::Enter);
    learning.flush().unwrap();
    assert_eq!(learning.read(|s| s.records().count()), Some(1));
    assert_eq!(
        learning.read(|s| {
            serde_json::to_value(s.records().next().unwrap()).unwrap()["selection_count"].as_u64()
        }),
        Some(Some(1))
    );
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn navigation_and_digit_choices_learn_only_the_final_committed_choice() {
    for digit in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let learning = LearningService::start(
            temp.path().join("learning.json"),
            temp.path().join("absent.hsd"),
        )
        .unwrap();
        let mut editor = Editor::new(&temp.path().join("first"), learning.clone());
        editor.convert();
        let chosen = editor.last.candidates.as_ref().unwrap().items[2]
            .value
            .clone();
        if digit {
            editor.key("3", SpecialKey::None);
        } else {
            editor.key("", SpecialKey::Down);
            editor.key("", SpecialKey::Down);
            learning.flush().unwrap();
            assert_eq!(learning.read(|s| s.records().count()), Some(0));
            editor.key("", SpecialKey::Enter);
        }
        learning.flush().unwrap();
        assert_eq!(learning.read(|s| s.records().count()), Some(1));
        assert_eq!(
            learning.read(|s| s.records().next().unwrap().correction.accepted.clone()),
            Some(chosen.clone())
        );
        drop(editor);
        let mut editor = Editor::new(&temp.path().join("second"), learning);
        editor.convert();
        assert_eq!(
            editor.last.candidates.as_ref().unwrap().items[0].value,
            chosen
        );
        assert_eq!(editor.last.candidates.as_ref().unwrap().selected, 0);
    }
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn cancellation_failed_ack_and_disabling_before_ack_do_not_learn() {
    for case in 0..4 {
        let temp = tempfile::tempdir().unwrap();
        let learning = LearningService::start(
            temp.path().join("learning.json"),
            temp.path().join("absent.hsd"),
        )
        .unwrap();
        let mut editor = Editor::new(&temp.path().join("mozc"), learning.clone());
        editor.convert();
        if case == 0 {
            editor.key("", SpecialKey::Down);
            editor.key("", SpecialKey::Escape);
            editor.key("", SpecialKey::Escape);
        } else {
            editor.choose_third();
            match case {
                1 => {
                    assert!(
                        editor
                            .engine
                            .handle(
                                editor.id,
                                Request {
                                    body: Some(request::Body::Ack(Ack {
                                        revision: editor.last.revision,
                                        event_seq: editor.last.event_seq,
                                        applied: false
                                    }))
                                }
                            )
                            .is_err()
                    );
                }
                2 => {
                    editor.engine.configure(kanade_engine::config::Config {
                        learning: false,
                        ..Default::default()
                    });
                    editor.acknowledge(true);
                }
                3 => editor.engine.close(editor.id),
                _ => unreachable!(),
            }
        }
        learning.flush().unwrap();
        assert_eq!(learning.read(|s| s.records().count()), Some(0));
    }
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn private_unknown_excluded_and_disabled_contexts_neither_use_nor_update_preferences() {
    let temp = tempfile::tempdir().unwrap();
    let learning = LearningService::start(
        temp.path().join("learning.json"),
        temp.path().join("absent.hsd"),
    )
    .unwrap();
    let mut training = Editor::new(&temp.path().join("training"), learning.clone());
    training.convert();
    let chosen = training.choose_third();
    training.acknowledge(true);
    learning.flush().unwrap();
    drop(training);
    for (index, (privacy, app, enabled)) in [
        (Privacy::Private, "com.example.Editor", true),
        (Privacy::Unknown, "com.example.Editor", true),
        (Privacy::Normal, "com.example.Excluded", true),
        (Privacy::Normal, "com.example.Editor", false),
        (Privacy::Normal, "", true),
    ]
    .into_iter()
    .enumerate()
    {
        let mut editor = Editor::new(
            &temp.path().join(format!("private-{index}")),
            learning.clone(),
        );
        editor.privacy = privacy;
        editor.app = app.into();
        editor.engine.configure(kanade_engine::config::Config {
            learning: enabled,
            learning_excluded_apps: vec!["com.example.Excluded".into()],
            ..Default::default()
        });
        editor.convert();
        assert_ne!(
            editor.last.candidates.as_ref().unwrap().items[0].value,
            chosen
        );
        editor.choose_third();
        editor.acknowledge(true);
        learning.flush().unwrap();
        assert_eq!(
            learning.read(|s| s
                .records()
                .map(|r| u64::from(r.selection_count))
                .sum::<u64>()),
            Some(1)
        );
    }
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn another_composition_keeps_its_order_while_learning_is_published() {
    let temp = tempfile::tempdir().unwrap();
    let learning = LearningService::start(
        temp.path().join("learning.json"),
        temp.path().join("absent.hsd"),
    )
    .unwrap();
    let mut current = Editor::new(&temp.path().join("current"), learning.clone());
    current.convert();
    let original: Vec<_> = current
        .last
        .candidates
        .as_ref()
        .unwrap()
        .items
        .iter()
        .map(|c| c.value.clone())
        .collect();
    let mut training = Editor::new(&temp.path().join("training"), learning.clone());
    training.convert();
    training.choose_third();
    training.acknowledge(true);
    learning.flush().unwrap();
    current.key("", SpecialKey::Down);
    assert_eq!(current.last.candidates.as_ref().unwrap().selected, 1);
    assert_eq!(
        current
            .last
            .candidates
            .as_ref()
            .unwrap()
            .items
            .iter()
            .map(|c| c.value.clone())
            .collect::<Vec<_>>(),
        original
    );
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn a_multisegment_commit_records_only_the_explicitly_selected_term() {
    let temp = tempfile::tempdir().unwrap();
    let learning = LearningService::start(
        temp.path().join("learning.json"),
        temp.path().join("absent.hsd"),
    )
    .unwrap();
    let mut editor = Editor::new(&temp.path().join("mozc"), learning.clone());
    for ch in "nihongonyuuryoku".chars() {
        editor.key(&ch.to_string(), SpecialKey::None);
    }
    editor.key("", SpecialKey::Space);
    assert!(editor.last.preedit.as_ref().unwrap().segments.len() >= 2);
    editor.key("", SpecialKey::Right);
    let term = editor.last.candidates.as_ref().unwrap().items[2].clone();
    editor.choose_third();
    editor.acknowledge(true);
    learning.flush().unwrap();
    assert_eq!(learning.read(|s| s.records().count()), Some(1));
    assert_eq!(
        learning.read(|s| s.records().next().unwrap().correction.accepted.clone()),
        Some(term.value)
    );
}
