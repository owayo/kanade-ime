// SPDX-License-Identifier: MIT
//! Engine contracts for keeping sessions alive and for rebuilding a lost
//! composition, with a scripted conversion backend. No worker is involved.
use kanade_engine::Engine;
use kanade_ipc::*;
use kanade_mozc::{ConversionEngine, Error, Result, SessionId, Snapshot};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

const ALLOWED: &str = "com.apple.TextEdit";

/// Every call the engine makes, in order.
type Calls = Arc<Mutex<Vec<String>>>;
#[derive(Default)]
struct Backend {
    calls: Calls,
    next: u64,
    readings: HashMap<SessionId, String>,
}
impl Backend {
    fn log(&self, call: impl Into<String>) {
        self.calls.lock().unwrap().push(call.into());
    }
    fn snapshot(&self, id: SessionId) -> Snapshot {
        let reading = &self.readings[&id];
        Snapshot {
            consumed: true,
            preedit: Preedit {
                cursor_utf16: utf16_len(reading),
                segments: if reading.is_empty() {
                    vec![]
                } else {
                    vec![Segment {
                        reading: reading.clone(),
                        value: reading.clone(),
                        focused: false,
                        value_utf16: utf16_len(reading),
                    }]
                },
            },
            ..Default::default()
        }
    }
}
impl ConversionEngine for Backend {
    fn add_history(&mut self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn create_session(&mut self) -> Result<SessionId> {
        self.next += 1;
        let id = SessionId::scripted(self.next);
        self.readings.insert(id, String::new());
        Ok(id)
    }
    fn delete_session(&mut self, id: SessionId) -> Result<()> {
        self.log("delete");
        self.readings.remove(&id);
        Ok(())
    }
    fn set_privacy(&mut self, privacy: Privacy) -> Result<()> {
        self.log(format!("privacy:{privacy:?}"));
        Ok(())
    }
    fn key(&mut self, id: SessionId, key: &Key, _: &str) -> Result<Snapshot> {
        self.log("key");
        let reading = self.readings.get_mut(&id).ok_or(Error::Protocol)?;
        if key.special == SpecialKey::Enter as i32 {
            let committed = std::mem::take(reading);
            return Ok(Snapshot {
                consumed: !committed.is_empty(),
                undo_mozc_commit: !committed.is_empty(),
                commit: (!committed.is_empty()).then(|| Commit {
                    reading: committed.clone(),
                    value: committed,
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        reading.push_str(&key.text);
        Ok(self.snapshot(id))
    }
    fn select(&mut self, _: SessionId, _: &str) -> Result<Snapshot> {
        Err(Error::StaleCandidate)
    }
    fn delete_candidate_history(&mut self, _: SessionId, _: &str) -> Result<Snapshot> {
        Err(Error::StaleCandidate)
    }
    fn submit(&mut self, _: SessionId) -> Result<Snapshot> {
        Err(Error::Protocol)
    }
    fn reset(&mut self, id: SessionId) -> Result<()> {
        self.log("reset");
        self.readings.insert(id, String::new());
        Ok(())
    }
    fn undo_learning(&mut self, _: SessionId) -> Result<()> {
        self.log("undo");
        Ok(())
    }
    fn sync(&mut self) -> Result<()> {
        Ok(())
    }
    fn restore(&mut self, id: SessionId, reading: &str) -> Result<Snapshot> {
        self.log(format!("restore:{reading}"));
        if reading.contains('変') {
            // The worker would not reproduce this text as typed.
            return Err(Error::Unrestorable);
        }
        self.readings.insert(id, reading.to_owned());
        Ok(self.snapshot(id))
    }
}

struct Peer {
    connection: u64,
    revision: Revision,
}
fn engine() -> (Engine<Backend>, Calls) {
    let backend = Backend::default();
    let calls = backend.calls.clone();
    (Engine::new(backend, None), calls)
}
fn open(engine: &mut Engine<Backend>, max_version: u32) -> Peer {
    let welcome = engine
        .open(&Hello {
            min_version: 1,
            max_version,
        })
        .unwrap();
    assert_eq!(welcome.version, max_version.min(CURRENT_VERSION));
    Peer {
        connection: welcome.connection,
        revision: Revision {
            connection: welcome.connection,
            context: 1,
            composition: 0,
        },
    }
}
fn private() -> Context {
    Context {
        privacy: Privacy::Private as i32,
        ..Default::default()
    }
}
fn restore(revision: Revision, seq: u64, reading: &str, context: Context) -> Request {
    Request {
        body: Some(request::Body::Restore(RestoreRequest {
            revision: Some(revision),
            event_seq: seq,
            reading: reading.into(),
            context: Some(context),
        })),
    }
}
fn key(revision: Revision, seq: u64, key: Key, passed_through: bool) -> Request {
    Request {
        body: Some(request::Body::Key(KeyRequest {
            revision: Some(revision),
            event_seq: seq,
            key: Some(key),
            context: Some(private()),
            passed_through,
        })),
    }
}
fn typed(text: &str) -> Key {
    Key {
        text: text.into(),
        ..Default::default()
    }
}
fn enter() -> Key {
    Key {
        special: SpecialKey::Enter as i32,
        ..Default::default()
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
fn reply(result: std::result::Result<Option<Response>, failure::Code>) -> KeyReply {
    match result.unwrap().unwrap().body.unwrap() {
        response::Body::Key(reply) => reply,
        _ => panic!("expected a key reply"),
    }
}
fn count(calls: &Calls, prefix: &str) -> usize {
    let calls = calls.lock().unwrap();
    calls.iter().filter(|call| call.starts_with(prefix)).count()
}

#[test]
fn restore_rebuilds_the_reading_without_a_commit_and_follows_event_rules() {
    let (mut engine, calls) = engine();
    let peer = open(&mut engine, CURRENT_VERSION);
    let request = restore(peer.revision, 1, "かんｋ", private());
    let restored = reply(engine.handle(peer.connection, request.clone()));
    assert_eq!(restored.event_seq, 1);
    assert_eq!(restored.revision.unwrap().composition, 1);
    assert_eq!(reading(restored.preedit.as_ref().unwrap()), "かんｋ");
    assert!(restored.commit.is_none() && restored.candidates.is_none());
    let policy = restored.key_policy.unwrap();
    assert!(policy.composing && !policy.can_undo_learning);
    // The same request again returns the cached reply; the worker is not asked twice.
    assert_eq!(reply(engine.handle(peer.connection, request)), restored);
    assert_eq!(count(&calls, "restore"), 1);
    // A different request under the same number is not a retry.
    assert_eq!(
        engine.handle(
            peer.connection,
            restore(peer.revision, 1, "かん", private())
        ),
        Err(failure::Code::Sequence)
    );
    // The restored edit must be acknowledged before anything else.
    let revision = restored.revision.unwrap();
    assert_eq!(
        engine.handle(peer.connection, key(revision, 2, typed("a"), false)),
        Err(failure::Code::Sequence)
    );
    assert_eq!(
        engine.handle(peer.connection, ack(&restored, true)),
        Ok(None)
    );
    assert!(engine.composing());
    let next = reply(engine.handle(peer.connection, key(revision, 2, typed("a"), false)));
    assert_eq!(reading(next.preedit.as_ref().unwrap()), "かんｋa");
    assert_eq!(engine.handle(peer.connection, ack(&next, true)), Ok(None));
    // A session that already has events cannot adopt another reading.
    assert_eq!(
        engine.handle(
            peer.connection,
            restore(next.revision.unwrap(), 3, "かん", private())
        ),
        Err(failure::Code::Sequence)
    );
    assert_eq!(count(&calls, "restore"), 1);
    assert_eq!(count(&calls, "undo"), 0);
}

#[test]
fn restore_requires_the_negotiated_version() {
    let (mut engine, calls) = engine();
    for version in [1, 2] {
        let peer = open(&mut engine, version);
        assert_eq!(
            engine.handle(
                peer.connection,
                restore(peer.revision, 1, "かんじ", private())
            ),
            Err(failure::Code::Invalid)
        );
        // The refusal changes nothing: the session takes keys as before.
        let typed = reply(engine.handle(peer.connection, key(peer.revision, 1, typed("a"), false)));
        assert_eq!(reading(typed.preedit.as_ref().unwrap()), "a");
    }
    assert_eq!(count(&calls, "restore"), 0);
}

#[test]
fn restore_rejects_other_epochs_and_later_events() {
    let (mut engine, calls) = engine();
    let peer = open(&mut engine, CURRENT_VERSION);
    let other = open(&mut engine, CURRENT_VERSION);
    for (revision, seq, expected) in [
        (other.revision, 1, failure::Code::Stale),
        (
            Revision {
                context: 2,
                ..peer.revision
            },
            1,
            failure::Code::Stale,
        ),
        (
            Revision {
                composition: 1,
                ..peer.revision
            },
            1,
            failure::Code::Stale,
        ),
        (peer.revision, 2, failure::Code::Sequence),
        (peer.revision, 0, failure::Code::Sequence),
    ] {
        assert_eq!(
            engine.handle(peer.connection, restore(revision, seq, "かんじ", private())),
            Err(expected)
        );
    }
    let first = reply(engine.handle(peer.connection, key(peer.revision, 1, typed("a"), false)));
    engine.handle(peer.connection, ack(&first, true)).unwrap();
    assert_eq!(
        engine.handle(
            peer.connection,
            restore(first.revision.unwrap(), 2, "かんじ", private())
        ),
        Err(failure::Code::Sequence)
    );
    assert_eq!(count(&calls, "restore"), 0);
}

#[test]
fn restore_switches_learning_off_before_text_reaches_the_worker() {
    for (context, expected) in [
        (private(), Some("privacy:Private")),
        (Context::default(), Some("privacy:Unknown")),
        // Explicit exclusions and unidentified applications fail closed.
        (
            Context {
                privacy: Privacy::Normal as i32,
                app: "org.example.other".into(),
                ..Default::default()
            },
            Some("privacy:Unknown"),
        ),
        (
            Context {
                privacy: Privacy::Normal as i32,
                ..Default::default()
            },
            Some("privacy:Unknown"),
        ),
        (
            Context {
                privacy: Privacy::Normal as i32,
                app: "org.example.new".into(),
                ..Default::default()
            },
            Some("privacy:Normal"),
        ),
        (
            Context {
                privacy: Privacy::Normal as i32,
                app: ALLOWED.into(),
                ..Default::default()
            },
            Some("privacy:Normal"),
        ),
        (
            Context {
                privacy: Privacy::Password as i32,
                ..Default::default()
            },
            None,
        ),
    ] {
        let (mut engine, calls) = engine();
        engine.configure(kanade_engine::config::Config {
            learning_excluded_apps: vec!["org.example.other".into()],
            ..Default::default()
        });
        let peer = open(&mut engine, CURRENT_VERSION);
        calls.lock().unwrap().clear();
        let result = engine.handle(
            peer.connection,
            restore(peer.revision, 1, "かんじ", context),
        );
        let calls = calls.lock().unwrap();
        match expected {
            Some(privacy) => {
                assert!(result.is_ok());
                assert_eq!(*calls, [privacy, "restore:かんじ"]);
            }
            None => {
                assert_eq!(result, Err(failure::Code::Privacy));
                assert!(calls.is_empty());
            }
        }
    }
}

#[test]
fn unusable_readings_are_refused_and_leave_the_session_usable() {
    let (mut engine, calls) = engine();
    let peer = open(&mut engine, CURRENT_VERSION);
    let long = "あ".repeat(MAX_PREEDIT / 3 + 1);
    for reading in ["", "か\u{0}", "か\nん", long.as_str()] {
        assert_eq!(
            engine.handle(
                peer.connection,
                restore(peer.revision, 1, reading, private())
            ),
            Err(failure::Code::Invalid)
        );
    }
    assert_eq!(count(&calls, "restore"), 0);
    // The worker could not reproduce this reading as typed.
    assert_eq!(
        engine.handle(
            peer.connection,
            restore(peer.revision, 1, "変換", private())
        ),
        Err(failure::Code::Invalid)
    );
    assert!(!engine.composing());
    assert!(engine.worker_healthy());
    let typed = reply(engine.handle(peer.connection, key(peer.revision, 1, typed("a"), false)));
    assert_eq!(reading(typed.preedit.as_ref().unwrap()), "a");
}

#[test]
fn a_failure_in_one_session_keeps_the_worker_and_other_compositions() {
    let (mut engine, calls) = engine();
    let first = open(&mut engine, CURRENT_VERSION);
    let second = open(&mut engine, CURRENT_VERSION);
    let kept = reply(engine.handle(
        second.connection,
        key(second.revision, 1, typed("か"), false),
    ));
    engine.handle(second.connection, ack(&kept, true)).unwrap();
    // The first frontend's document rejected its edit. That session ends.
    let rejected =
        reply(engine.handle(first.connection, key(first.revision, 1, typed("あ"), false)));
    calls.lock().unwrap().clear();
    assert_eq!(
        engine.handle(first.connection, ack(&rejected, false)),
        Err(failure::Code::Unavailable)
    );
    assert_eq!(*calls.lock().unwrap(), ["reset", "delete"]);
    // The worker is not replaced for it, so the other composition goes on.
    assert!(engine.worker_healthy());
    assert!(engine.composing());
    let more = reply(engine.handle(
        second.connection,
        key(kept.revision.unwrap(), 2, typed("な"), false),
    ));
    assert_eq!(reading(more.preedit.as_ref().unwrap()), "かな");
    assert_eq!(
        engine.handle(first.connection, key(first.revision, 2, typed("a"), false)),
        Err(failure::Code::Stale)
    );
}

#[test]
fn a_key_left_to_the_application_cannot_start_a_composition() {
    let (mut engine, calls) = engine();
    let peer = open(&mut engine, CURRENT_VERSION);
    // Sending this to the scripted worker would start a composition.
    let passed = reply(engine.handle(peer.connection, key(peer.revision, 1, typed("¥"), true)));
    assert!(!passed.consumed);
    assert!(passed.commit.is_none());
    assert!(passed.preedit.unwrap().segments.is_empty());
    assert_eq!(count(&calls, "key"), 0);
    assert_eq!(count(&calls, "reset"), 0);
    assert!(engine.worker_healthy());
    engine
        .handle(
            peer.connection,
            ack(
                &KeyReply {
                    preedit: None,
                    ..passed
                },
                true,
            ),
        )
        .unwrap();
    assert!(!engine.composing());
    let typed = reply(engine.handle(
        peer.connection,
        key(passed.revision.unwrap(), 2, typed("か"), false),
    ));
    assert_eq!(reading(typed.preedit.as_ref().unwrap()), "か");
}

#[test]
fn closing_with_an_unacknowledged_commit_takes_back_what_the_worker_learned() {
    for acknowledged in [false, true] {
        let (mut engine, calls) = engine();
        let peer = open(&mut engine, CURRENT_VERSION);
        let typed =
            reply(engine.handle(peer.connection, key(peer.revision, 1, typed("か"), false)));
        engine.handle(peer.connection, ack(&typed, true)).unwrap();
        let committed = reply(engine.handle(
            peer.connection,
            key(typed.revision.unwrap(), 2, enter(), false),
        ));
        assert_eq!(committed.commit.as_ref().unwrap().value, "か");
        if acknowledged {
            engine
                .handle(peer.connection, ack(&committed, true))
                .unwrap();
        }
        calls.lock().unwrap().clear();
        // The frontend gave up waiting, or went away, and never acknowledged.
        engine.close(peer.connection);
        let expected: &[&str] = if acknowledged {
            &["delete"]
        } else {
            &["undo", "delete"]
        };
        assert_eq!(*calls.lock().unwrap(), expected);
    }
}
