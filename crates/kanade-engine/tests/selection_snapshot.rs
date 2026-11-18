//! A publication racing the backend selection cannot split preedit and order.
use kanade_engine::Engine;
use kanade_ipc::*;
use kanade_learn::service::LearningService;
use kanade_mozc::{ConversionEngine, FocusCandidate, Result, SessionId, Snapshot};

struct Backend {
    learning: LearningService,
    composing: bool,
    converting: bool,
    selected: String,
    injected: bool,
}
impl Backend {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            consumed: true,
            converting: self.converting,
            preedit: Preedit {
                cursor_utf16: utf16_len(&self.selected),
                segments: if self.composing {
                    vec![Segment {
                        reading: "こうせい".into(),
                        value: self.selected.clone(),
                        focused: true,
                        value_utf16: utf16_len(&self.selected),
                    }]
                } else {
                    vec![]
                },
            },
            candidates: if self.converting {
                ["構成", "校正", "公正"]
                    .into_iter()
                    .map(|value| FocusCandidate {
                        value: value.into(),
                        reading: "こうせい".into(),
                        annotation: String::new(),
                        transliteration: false,
                        history_deletable: false,
                    })
                    .collect()
            } else {
                vec![]
            },
            ..Default::default()
        }
    }
}
impl ConversionEngine for Backend {
    fn create_session(&mut self) -> Result<SessionId> {
        Ok(SessionId::scripted(1))
    }
    fn delete_session(&mut self, _: SessionId) -> Result<()> {
        Ok(())
    }
    fn set_privacy(&mut self, _: Privacy) -> Result<()> {
        Ok(())
    }
    fn key(&mut self, _: SessionId, key: &Key, _: &str) -> Result<Snapshot> {
        self.composing = true;
        self.converting = key.special == SpecialKey::Space as i32;
        self.selected = if self.converting {
            "構成"
        } else {
            "こうせい"
        }
        .into();
        Ok(self.snapshot())
    }
    fn select(&mut self, _: SessionId, value: &str) -> Result<Snapshot> {
        self.selected = value.into();
        if !self.injected {
            self.injected = true;
            assert!(self.learning.learned(
                2,
                vec![],
                vec![Word {
                    reading: "こうせい".into(),
                    value: "公正".into()
                }],
                0
            ));
            self.learning.flush().unwrap();
        }
        Ok(self.snapshot())
    }
    fn submit(&mut self, _: SessionId) -> Result<Snapshot> {
        self.composing = false;
        self.converting = false;
        Ok(Snapshot {
            consumed: true,
            commit: Some(Commit {
                reading: "こうせい".into(),
                value: self.selected.clone(),
                words: vec![Word {
                    reading: "こうせい".into(),
                    value: self.selected.clone(),
                }],
                ..Default::default()
            }),
            ..Default::default()
        })
    }
    fn delete_candidate_history(&mut self, _: SessionId, _: &str) -> Result<Snapshot> {
        Err(kanade_mozc::Error::StaleCandidate)
    }
    fn reset(&mut self, _: SessionId) -> Result<()> {
        self.composing = false;
        self.converting = false;
        Ok(())
    }
    fn undo_learning(&mut self, _: SessionId) -> Result<()> {
        Ok(())
    }
    fn sync(&mut self) -> Result<()> {
        Ok(())
    }
    fn add_history(&mut self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
}

#[test]
fn one_key_uses_one_preference_snapshot_for_selection_and_order() {
    let temp = tempfile::tempdir().unwrap();
    let learning = LearningService::start(
        temp.path().join("learning.json"),
        temp.path().join("absent.hsd"),
    )
    .unwrap();
    learning.learned(
        1,
        vec![],
        vec![Word {
            reading: "こうせい".into(),
            value: "校正".into(),
        }],
        0,
    );
    learning.flush().unwrap();
    let mut engine = Engine::new(
        Backend {
            learning: learning.clone(),
            composing: false,
            converting: false,
            selected: String::new(),
            injected: false,
        },
        Some(learning),
    );
    let welcome = engine
        .open(&Hello {
            min_version: 2,
            max_version: 2,
        })
        .unwrap();
    let mut revision = Revision {
        connection: welcome.connection,
        context: 1,
        composition: 0,
    };
    for (i, (text, key)) in [
        ("a", SpecialKey::None),
        ("", SpecialKey::Space),
        ("", SpecialKey::Enter),
        ("a", SpecialKey::None),
        ("", SpecialKey::Space),
    ]
    .into_iter()
    .enumerate()
    {
        let seq = i as u64 + 1;
        let response = engine
            .handle(
                welcome.connection,
                Request {
                    body: Some(request::Body::Key(KeyRequest {
                        revision: Some(revision),
                        event_seq: seq,
                        key: Some(Key {
                            text: text.into(),
                            special: key as i32,
                            ..Default::default()
                        }),
                        context: Some(Context {
                            privacy: Privacy::Normal as i32,
                            app: "com.apple.TextEdit".into(),
                            ..Default::default()
                        }),
                        passed_through: false,
                    })),
                },
            )
            .unwrap()
            .unwrap();
        let Some(response::Body::Key(reply)) = response.body else {
            panic!("key reply required")
        };
        revision = reply.revision.unwrap();
        if key == SpecialKey::Space {
            let wanted = if i == 1 { "校正" } else { "公正" };
            assert_eq!(surface(reply.preedit.as_ref().unwrap()), wanted);
            assert_eq!(reply.candidates.as_ref().unwrap().items[0].value, wanted);
            assert_eq!(reply.candidates.as_ref().unwrap().selected, 0);
        }
        engine
            .handle(
                welcome.connection,
                Request {
                    body: Some(request::Body::Ack(Ack {
                        revision: Some(revision),
                        event_seq: seq,
                        applied: true,
                    })),
                },
            )
            .unwrap();
    }
}
