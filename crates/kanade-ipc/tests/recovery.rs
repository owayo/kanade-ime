use kanade_ipc::{frontend::*, *};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    io,
    rc::Rc,
};
struct Script {
    replies: VecDeque<io::Result<Response>>,
    acks: usize,
}
impl Transport for Script {
    fn exchange(&mut self, r: &Request) -> io::Result<Response> {
        if matches!(r.body, Some(request::Body::Hello(_))) {
            return Ok(Response {
                body: Some(response::Body::Welcome(Welcome {
                    version: 2,
                    connection: 1,
                    learning_apps: vec![],
                    learning_policy: None,
                })),
            });
        }
        self.replies
            .pop_front()
            .unwrap_or_else(|| Err(io::ErrorKind::BrokenPipe.into()))
    }
    fn notify(&mut self, _: &Request) -> io::Result<()> {
        self.acks += 1;
        Ok(())
    }
}
#[derive(Default)]
struct Doc {
    commits: Vec<String>,
    marked: String,
    fail: bool,
    reject_after_apply: bool,
}
impl Document for Doc {
    fn apply(&mut self, r: &KeyReply) -> bool {
        if self.fail {
            return false;
        }
        if let Some(c) = &r.commit {
            self.commits.push(c.value.clone());
        }
        self.marked = r.preedit.as_ref().map(surface).unwrap_or_default();
        !self.reject_after_apply
    }
    fn finish_reading(&mut self, r: &str) -> bool {
        if self.fail {
            return false;
        }
        self.commits.push(r.into());
        self.marked.clear();
        true
    }
}
fn response(
    seq: u64,
    context: u64,
    reading: &str,
    value: &str,
    commit: Option<&str>,
) -> io::Result<Response> {
    Ok(Response {
        body: Some(response::Body::Key(KeyReply {
            revision: Some(Revision {
                connection: 1,
                context,
                composition: seq,
            }),
            event_seq: seq,
            state_revision: seq,
            consumed: true,
            preedit: Some(Preedit {
                segments: if value.is_empty() {
                    vec![]
                } else {
                    vec![Segment {
                        reading: reading.into(),
                        value: value.into(),
                        ..Default::default()
                    }]
                },
                ..Default::default()
            }),
            commit: commit.map(|s| Commit {
                value: s.into(),
                ..Default::default()
            }),
            candidates: None,
            key_policy: Some(KeyPolicySummary {
                enabled: true,
                composing: !value.is_empty(),
                ..Default::default()
            }),
        })),
    })
}
fn key() -> Key {
    Key {
        text: "a".into(),
        ..Default::default()
    }
}
#[test]
fn old_response_never_applies_to_another_context() {
    let mut client = Client::connect(Script {
        replies: VecDeque::from([response(1, 2, "あ", "あ", Some("誤"))]),
        acks: 0,
    })
    .unwrap();
    let mut doc = Doc::default();
    assert!(!client.key(key(), Context::default(), false, &mut doc));
    assert!(doc.commits.is_empty());
}
#[test]
fn failure_after_commit_does_not_repeat_the_commit() {
    let mut client = Client::connect(Script {
        replies: VecDeque::from([
            response(1, 1, "", "", Some("漢字")),
            Err(io::ErrorKind::BrokenPipe.into()),
        ]),
        acks: 0,
    })
    .unwrap();
    let mut doc = Doc::default();
    assert!(client.key(key(), Context::default(), false, &mut doc));
    assert!(!client.key(key(), Context::default(), false, &mut doc));
    assert_eq!(doc.commits, vec!["漢字"]);
}
#[test]
fn failed_document_edit_blocks_following_keys() {
    let mut client = Client::connect(Script {
        replies: VecDeque::from([response(1, 1, "あ", "あ", None)]),
        acks: 0,
    })
    .unwrap();
    let mut doc = Doc {
        fail: true,
        ..Default::default()
    };
    assert!(client.key(key(), Context::default(), false, &mut doc));
    assert!(client.key(key(), Context::default(), false, &mut doc));
    assert!(!client.can_reconnect());
    assert!(!client.key(
        key(),
        Context {
            privacy: Privacy::Password as i32,
            ..Default::default()
        },
        false,
        &mut doc
    ));
    assert!(doc.commits.is_empty());
}

#[test]
fn source_change_commits_conversion_once() {
    let mut client = Client::connect(Script {
        replies: VecDeque::from([
            response(1, 1, "かんじ", "漢字", None),
            response(2, 1, "", "", Some("漢字")),
        ]),
        acks: 0,
    })
    .unwrap();
    let mut doc = Doc::default();
    client.key(key(), Context::default(), false, &mut doc);
    assert!(client.commit_for_source_change(Context::default(), &mut doc));
    assert!(client.commit_for_source_change(Context::default(), &mut doc));
    assert_eq!(doc.commits, ["漢字"]);
    assert!(doc.marked.is_empty());
}

#[test]
fn source_change_refuses_and_does_not_replay_an_uncertain_edit() {
    let mut client = Client::connect(Script {
        replies: VecDeque::from([
            response(1, 1, "かんじ", "漢字", None),
            response(2, 1, "", "", Some("漢字")),
        ]),
        acks: 0,
    })
    .unwrap();
    let mut doc = Doc::default();
    client.key(key(), Context::default(), false, &mut doc);
    doc.reject_after_apply = true;
    assert!(!client.commit_for_source_change(Context::default(), &mut doc));
    doc.reject_after_apply = false;
    assert!(!client.commit_for_source_change(Context::default(), &mut doc));
    let blocked = client.finish(&mut doc);
    assert_eq!(doc.commits, ["漢字"]);
    assert!(blocked);
}

#[test]
fn source_change_recovers_confirmed_reading_on_worker_loss() {
    let mut client = Client::connect(Script {
        replies: VecDeque::from([
            response(1, 1, "かんじ", "漢字", None),
            Err(io::ErrorKind::BrokenPipe.into()),
        ]),
        acks: 0,
    })
    .unwrap();
    let mut doc = Doc::default();
    client.key(key(), Context::default(), false, &mut doc);
    assert!(client.commit_for_source_change(Context::default(), &mut doc));
    assert_eq!(doc.commits, ["かんじ"]);
    assert!(doc.marked.is_empty());
}

#[test]
fn source_change_in_password_context_bypasses_the_document_and_engine() {
    let mut client = Client::connect(Script {
        replies: VecDeque::from([response(1, 1, "かんじ", "漢字", None)]),
        acks: 0,
    })
    .unwrap();
    let mut doc = Doc::default();
    client.key(key(), Context::default(), false, &mut doc);
    assert!(client.commit_for_source_change(
        Context {
            privacy: Privacy::Password as i32,
            ..Default::default()
        },
        &mut doc
    ));
    assert_eq!(doc.marked, "漢字");
    assert!(doc.commits.is_empty());
    assert!(!client.is_connected());
}

#[test]
fn conversion_shortcuts_are_owned_only_during_composition() {
    let mut client = Client::connect(Script {
        replies: VecDeque::from([
            response(1, 1, "かな", "かな", None),
            response(2, 1, "", "", Some("かな")),
        ]),
        acks: 0,
    })
    .unwrap();
    for number in 6..=10 {
        let key = function_key(number, 0).unwrap();
        assert!(!client.would_consume(&key, Privacy::Normal));
        for modifier in [MOD_SHIFT, MOD_CONTROL, MOD_ALT, MOD_META] {
            assert!(function_key(number, modifier).is_none());
        }
    }
    let mut doc = Doc::default();
    client.key(key(), Context::default(), false, &mut doc);
    for number in 6..=10 {
        let key = function_key(number, 0).unwrap();
        assert!(client.would_consume(&key, Privacy::Normal));
        assert!(client.would_consume(&key, Privacy::Private));
        assert!(!client.would_consume(&key, Privacy::Password));
    }
    for modifier in [MOD_CONTROL, MOD_META] {
        assert!(!client.would_consume(
            &Key {
                text: "c".into(),
                modifiers: modifier,
                ..Default::default()
            },
            Privacy::Normal
        ));
    }
    assert!(client.commit_for_source_change(Context::default(), &mut doc));
    for number in 6..=10 {
        assert!(!client.would_consume(&function_key(number, 0).unwrap(), Privacy::Normal));
    }
}

#[test]
fn history_deletion_matches_only_control_forward_delete() {
    let deletion = Key {
        special: SpecialKey::Delete as i32,
        modifiers: MOD_CONTROL,
        ..Default::default()
    };
    assert!(is_history_delete(&deletion));
    for modifiers in [
        0,
        MOD_SHIFT,
        MOD_ALT,
        MOD_META,
        MOD_CONTROL | MOD_SHIFT,
        MOD_CONTROL | MOD_ALT,
    ] {
        assert!(!is_history_delete(&Key {
            modifiers,
            ..deletion.clone()
        }));
    }
    assert!(!is_history_delete(&Key {
        special: SpecialKey::Backspace as i32,
        ..deletion.clone()
    }));
    let mut client = Client::connect(Script {
        replies: VecDeque::from([response(1, 1, "かな", "かな", None)]),
        acks: 0,
    })
    .unwrap();
    assert!(!client.would_consume(&deletion, Privacy::Normal));
    client.key(key(), Context::default(), false, &mut Doc::default());
    assert!(client.would_consume(&deletion, Privacy::Normal));
    assert!(!client.would_consume(&deletion, Privacy::Password));
}

// A lost session must not insert text.
//
// The client used to call `finish_reading` as soon as an exchange failed, and
// to hand the failed key to the application. A slow reply or a closed
// connection therefore committed the kana the user was still converting and
// left a stray letter after it. These contracts replace the test that
// asserted that behaviour (`timeout_finishes_only_last_applied_reading`).

/// A connection whose requests and liveness the test can observe.
struct Wire {
    version: u32,
    connection: u64,
    replies: VecDeque<io::Result<Response>>,
    sent: Rc<RefCell<Vec<Request>>>,
    open: Rc<Cell<bool>>,
    lose_acks: bool,
}
impl Wire {
    fn new(connection: u64, replies: Vec<io::Result<Response>>) -> Self {
        Self {
            version: CURRENT_VERSION,
            connection,
            replies: replies.into(),
            sent: Default::default(),
            open: Rc::new(Cell::new(true)),
            lose_acks: false,
        }
    }
}
impl Transport for Wire {
    fn exchange(&mut self, request: &Request) -> io::Result<Response> {
        if let Some(request::Body::Hello(hello)) = &request.body {
            assert!(hello.min_version <= self.version && self.version <= hello.max_version);
            return Ok(Response {
                body: Some(response::Body::Welcome(Welcome {
                    version: self.version,
                    connection: self.connection,
                    learning_apps: vec![],
                    learning_policy: None,
                })),
            });
        }
        self.sent.borrow_mut().push(request.clone());
        self.replies
            .pop_front()
            .unwrap_or_else(|| Err(io::ErrorKind::BrokenPipe.into()))
    }
    fn notify(&mut self, request: &Request) -> io::Result<()> {
        if self.lose_acks {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        self.sent.borrow_mut().push(request.clone());
        Ok(())
    }
    fn is_open(&mut self) -> bool {
        self.open.get()
    }
}
fn on(connection: u64, reply: io::Result<Response>) -> io::Result<Response> {
    reply.map(|mut response| {
        if let Some(response::Body::Key(reply)) = response.body.as_mut() {
            reply.revision.as_mut().unwrap().connection = connection;
        }
        response
    })
}
fn failure(code: failure::Code) -> io::Result<Response> {
    Ok(Response {
        body: Some(response::Body::Failure(Failure { code: code as i32 })),
    })
}
fn typed(text: &str) -> Key {
    Key {
        text: text.into(),
        ..Default::default()
    }
}
fn special(key: SpecialKey) -> Key {
    Key {
        special: key as i32,
        ..Default::default()
    }
}
fn private() -> Context {
    Context {
        privacy: Privacy::Private as i32,
        ..Default::default()
    }
}
/// A client showing 漢字 for かんじ whose next exchange fails with `error`.
fn held_after(error: io::Result<Response>) -> (Client<Wire>, Doc) {
    let wire = Wire::new(1, vec![response(1, 1, "かんじ", "漢字", None), error]);
    let mut client = Client::connect(wire).unwrap();
    let mut doc = Doc::default();
    assert!(client.key(typed("a"), private(), false, &mut doc));
    // The failed key is used up: the application must not receive it either.
    assert!(client.key(typed("x"), private(), false, &mut doc));
    assert!(client.is_held());
    (client, doc)
}
fn restores(
    client: &mut Client<Wire>,
    replies: Vec<io::Result<Response>>,
) -> Rc<RefCell<Vec<Request>>> {
    let wire = Wire::new(2, replies);
    let sent = wire.sent.clone();
    assert!(client.needs_transport());
    client.reattach(wire).unwrap();
    sent
}

#[test]
fn a_failed_exchange_holds_the_composition_and_inserts_nothing() {
    for error in [
        Err(io::ErrorKind::TimedOut.into()),
        Err(io::ErrorKind::BrokenPipe.into()),
        Err(io::ErrorKind::UnexpectedEof.into()),
        failure(failure::Code::Stale),
        failure(failure::Code::Unavailable),
        failure(failure::Code::Invalid),
        // A reply for another event is not this key's reply.
        response(9, 1, "かんじ", "漢字", Some("誤")),
    ] {
        let (mut client, doc) = held_after(error);
        assert!(doc.commits.is_empty());
        assert_eq!(doc.marked, "漢字");
        assert!(!client.is_connected());
        // A frontend that replaces its client would lose the composition.
        assert!(!client.can_reconnect());
        assert!(client.needs_transport());
        assert!(client.would_consume(&typed("a"), Privacy::Private));
    }
}

#[test]
fn neither_more_keys_nor_elapsed_time_end_a_hold() {
    let (mut client, mut doc) = held_after(Err(io::ErrorKind::TimedOut.into()));
    for _ in 0..3 {
        for key in [
            typed("k"),
            special(SpecialKey::Space),
            special(SpecialKey::Backspace),
        ] {
            assert!(client.key(key, private(), false, &mut doc));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(client.select(CandidateHandle::default(), &mut doc));
    assert!(client.is_held());
    assert!(doc.commits.is_empty());
    assert_eq!(doc.marked, "漢字");
}

#[test]
fn enter_ends_a_hold_with_the_acknowledged_reading() {
    let (mut client, mut doc) = held_after(Err(io::ErrorKind::TimedOut.into()));
    assert!(client.key(special(SpecialKey::Enter), private(), false, &mut doc));
    // The reading the document acknowledged, never the unapproved conversion.
    assert_eq!(doc.commits, ["かんじ"]);
    assert!(doc.marked.is_empty());
    assert!(!client.is_held());
    assert!(client.can_reconnect());
    // A second Enter belongs to the application.
    assert!(!client.key(special(SpecialKey::Enter), private(), false, &mut doc));
    assert_eq!(doc.commits, ["かんじ"]);
}

#[test]
fn escape_ends_a_hold_without_inserting() {
    let (mut client, mut doc) = held_after(Err(io::ErrorKind::TimedOut.into()));
    assert!(client.key(special(SpecialKey::Escape), private(), false, &mut doc));
    assert!(doc.commits.is_empty());
    assert!(doc.marked.is_empty());
    assert!(!client.is_held());
    assert!(client.can_reconnect());
}

#[test]
fn a_chord_left_to_the_application_does_not_end_a_hold() {
    let (mut client, mut doc) = held_after(Err(io::ErrorKind::TimedOut.into()));
    let chord = Key {
        text: "s".into(),
        modifiers: MOD_META,
        ..Default::default()
    };
    assert!(!client.would_consume(&chord, Privacy::Private));
    assert!(!client.key(chord, private(), true, &mut doc));
    assert!(client.is_held());
    assert!(doc.commits.is_empty());
    assert_eq!(doc.marked, "漢字");
}

#[test]
fn a_new_session_restores_the_reading_and_then_takes_the_next_key() {
    let (mut client, mut doc) = held_after(Err(io::ErrorKind::TimedOut.into()));
    let sent = restores(
        &mut client,
        vec![
            on(2, response(1, 1, "かんじ", "かんじ", None)),
            on(2, response(2, 1, "かんじあ", "かんじあ", None)),
        ],
    );
    assert!(!client.needs_transport());
    // Reattaching alone changes nothing the user can see.
    assert_eq!(doc.marked, "漢字");
    assert!(client.key(typed("y"), private(), false, &mut doc));
    assert!(!client.is_held());
    assert!(doc.commits.is_empty());
    assert_eq!(doc.marked, "かんじあ");
    let sent = sent.borrow();
    let fresh = Revision {
        connection: 2,
        context: 1,
        composition: 0,
    };
    let restored = Revision {
        composition: 1,
        ..fresh
    };
    assert_eq!(
        sent[0].body,
        Some(request::Body::Restore(RestoreRequest {
            revision: Some(fresh),
            event_seq: 1,
            reading: "かんじ".into(),
            context: Some(private()),
        }))
    );
    assert_eq!(
        sent[1].body,
        Some(request::Body::Ack(Ack {
            revision: Some(restored),
            event_seq: 1,
            applied: true,
        }))
    );
    // The key pressed now follows, numbered after the restore.
    let Some(request::Body::Key(key)) = &sent[2].body else {
        panic!("expected the new key");
    };
    assert_eq!((key.event_seq, key.revision), (2, Some(restored)));
    assert_eq!(key.key.as_ref().unwrap().text, "y");
    assert!(matches!(sent[3].body, Some(request::Body::Ack(_))));
    assert_eq!(sent.len(), 4);
    // The key whose exchange failed is never sent to the new session.
    assert!(sent.iter().all(|request| !matches!(
        &request.body,
        Some(request::Body::Key(key)) if key.key.as_ref().unwrap().text == "x"
    )));
}

#[test]
fn waiting_romaji_is_restored_as_part_of_the_acknowledged_reading() {
    let wire = Wire::new(
        1,
        vec![
            response(1, 1, "かｎ", "かｎ", None),
            Err(io::ErrorKind::TimedOut.into()),
        ],
    );
    let mut client = Client::connect(wire).unwrap();
    let mut doc = Doc::default();
    client.key(typed("n"), private(), false, &mut doc);
    client.key(typed("x"), private(), false, &mut doc);
    let sent = restores(
        &mut client,
        vec![
            on(2, response(1, 1, "かｎ", "かｎ", None)),
            on(2, response(2, 1, "かに", "かに", None)),
        ],
    );
    assert!(client.key(typed("i"), private(), false, &mut doc));
    assert_eq!(doc.marked, "かに");
    let Some(request::Body::Restore(restore)) = &sent.borrow()[0].body else {
        panic!("expected a restore");
    };
    assert_eq!(restore.reading, "かｎ");
}

#[test]
fn an_engine_without_restore_is_not_asked_and_the_hold_stays() {
    let (mut client, mut doc) = held_after(Err(io::ErrorKind::TimedOut.into()));
    let mut wire = Wire::new(2, vec![on(2, response(1, 1, "あ", "あ", None))]);
    wire.version = 2;
    let sent = wire.sent.clone();
    client.reattach(wire).unwrap();
    assert!(client.key(typed("y"), private(), false, &mut doc));
    assert!(sent.borrow().is_empty());
    assert!(client.is_held());
    assert!(!client.needs_transport());
    assert!(doc.commits.is_empty());
    assert_eq!(doc.marked, "漢字");
    // The user's Enter ends it. The connection then serves the next
    // composition from its first sequence number.
    assert!(client.key(special(SpecialKey::Enter), private(), false, &mut doc));
    assert_eq!(doc.commits, ["かんじ"]);
    let mut wire = Wire::new(3, vec![on(3, response(1, 1, "あ", "あ", None))]);
    wire.version = 2;
    assert!(client.needs_transport());
    client.reattach(wire).unwrap();
    assert!(client.key(typed("a"), private(), false, &mut doc));
    assert_eq!(doc.marked, "あ");
}

#[test]
fn a_refused_restore_leaves_the_composition_for_an_explicit_end() {
    for refusal in [
        failure(failure::Code::Invalid),
        failure(failure::Code::Sequence),
        failure(failure::Code::Privacy),
        // Another reading, a commit, or another event: none is applied.
        on(2, response(1, 1, "かんし", "かんし", None)),
        on(2, response(1, 1, "かんじ", "かんじ", Some("漢字"))),
        on(2, response(4, 1, "かんじ", "かんじ", None)),
        on(3, response(1, 1, "かんじ", "かんじ", None)),
        on(2, response(1, 2, "かんじ", "かんじ", None)),
    ] {
        let (mut client, mut doc) = held_after(Err(io::ErrorKind::TimedOut.into()));
        let sent = restores(&mut client, vec![refusal]);
        assert!(client.key(typed("y"), private(), false, &mut doc));
        assert_eq!(sent.borrow().len(), 1);
        assert!(client.is_held());
        assert!(doc.commits.is_empty());
        assert_eq!(doc.marked, "漢字");
        // Asking again cannot help; no further connection is requested.
        assert!(!client.needs_transport());
        for _ in 0..5 {
            assert!(client.key(typed("y"), private(), false, &mut doc));
        }
        assert!(doc.commits.is_empty());
        assert!(client.key(special(SpecialKey::Enter), private(), false, &mut doc));
        assert_eq!(doc.commits, ["かんじ"]);
        assert!(client.needs_transport());
    }
}

#[test]
fn repeated_restore_failures_stop_retrying_and_still_insert_nothing() {
    let (mut client, mut doc) = held_after(Err(io::ErrorKind::TimedOut.into()));
    let mut attempts = 0;
    while client.needs_transport() {
        let error = if attempts % 2 == 0 {
            Err(io::ErrorKind::TimedOut.into())
        } else {
            failure(failure::Code::Unavailable)
        };
        client
            .reattach(Wire::new(2 + attempts, vec![error]))
            .unwrap();
        assert!(client.key(typed("y"), private(), false, &mut doc));
        attempts += 1;
        assert!(attempts <= 3);
    }
    assert_eq!(attempts, 3);
    assert!(client.is_held());
    assert!(doc.commits.is_empty());
    assert_eq!(doc.marked, "漢字");
    assert!(client.key(special(SpecialKey::Escape), private(), false, &mut doc));
    assert!(doc.commits.is_empty());
    assert!(doc.marked.is_empty());
}

#[test]
fn a_restore_the_document_rejects_blocks_without_inserting() {
    let (mut client, mut doc) = held_after(Err(io::ErrorKind::TimedOut.into()));
    let sent = restores(
        &mut client,
        vec![on(2, response(1, 1, "かんじ", "かんじ", None))],
    );
    doc.fail = true;
    assert!(client.key(typed("y"), private(), false, &mut doc));
    doc.fail = false;
    assert_eq!(
        sent.borrow()[1].body,
        Some(request::Body::Ack(Ack {
            revision: Some(Revision {
                connection: 2,
                context: 1,
                composition: 1,
            }),
            event_seq: 1,
            applied: false,
        }))
    );
    assert_eq!(sent.borrow().len(), 2);
    assert!(!client.needs_transport());
    assert!(client.key(special(SpecialKey::Enter), private(), false, &mut doc));
    assert!(client.finish(&mut doc));
    assert!(doc.commits.is_empty());
}

#[test]
fn a_password_context_discards_a_hold_and_nothing_is_restored() {
    let (mut client, mut doc) = held_after(Err(io::ErrorKind::TimedOut.into()));
    let password = Context {
        privacy: Privacy::Password as i32,
        ..Default::default()
    };
    assert!(!client.key(typed("p"), password, false, &mut doc));
    assert!(!client.is_held());
    assert!(doc.commits.is_empty());
    let wire = Wire::new(2, vec![on(2, response(1, 1, "あ", "あ", None))]);
    let sent = wire.sent.clone();
    assert!(client.needs_transport());
    client.reattach(wire).unwrap();
    assert!(client.key(typed("a"), private(), false, &mut doc));
    assert!(matches!(sent.borrow()[0].body, Some(request::Body::Key(_))));
    assert!(doc.commits.is_empty());
}

#[test]
fn the_end_of_focus_or_source_finishes_a_hold_once() {
    let (mut client, mut doc) = held_after(Err(io::ErrorKind::TimedOut.into()));
    assert!(!client.finish(&mut doc));
    assert!(!client.finish(&mut doc));
    assert_eq!(doc.commits, ["かんじ"]);
    let (mut client, mut doc) = held_after(failure(failure::Code::Stale));
    assert!(client.commit_for_source_change(private(), &mut doc));
    assert!(client.commit_for_source_change(private(), &mut doc));
    assert_eq!(doc.commits, ["かんじ"]);
    assert!(doc.marked.is_empty());
}

#[test]
fn a_connection_closed_while_idle_is_replaced_before_the_key_is_sent() {
    // Nothing is displayed: the key simply goes to the new session.
    let wire = Wire::new(1, vec![]);
    let (open, old) = (wire.open.clone(), wire.sent.clone());
    let mut client = Client::connect(wire).unwrap();
    let mut doc = Doc::default();
    assert!(!client.needs_transport());
    open.set(false);
    assert!(client.needs_transport());
    assert!(!client.is_held());
    client
        .reattach(Wire::new(2, vec![on(2, response(1, 1, "あ", "あ", None))]))
        .unwrap();
    assert!(client.key(typed("a"), private(), false, &mut doc));
    assert_eq!(doc.marked, "あ");
    assert!(old.borrow().is_empty());

    // A composition is displayed: it is restored, then the key is processed.
    let wire = Wire::new(1, vec![response(1, 1, "かんじ", "漢字", None)]);
    let (open, old) = (wire.open.clone(), wire.sent.clone());
    let mut client = Client::connect(wire).unwrap();
    let mut doc = Doc::default();
    client.key(typed("a"), private(), false, &mut doc);
    let before = old.borrow().len();
    open.set(false);
    assert!(client.needs_transport());
    assert!(client.is_held());
    client
        .reattach(Wire::new(
            2,
            vec![
                on(2, response(1, 1, "かんじ", "かんじ", None)),
                on(2, response(2, 1, "", "", Some("漢字"))),
            ],
        ))
        .unwrap();
    assert!(client.key(special(SpecialKey::Space), private(), false, &mut doc));
    // No key was lost and none was sent on the closed connection.
    assert_eq!(old.borrow().len(), before);
    assert_eq!(doc.commits, ["漢字"]);
    assert!(doc.marked.is_empty());
}

#[test]
fn a_lost_acknowledgement_holds_what_the_document_shows() {
    let mut wire = Wire::new(1, vec![response(1, 1, "かんじ", "漢字", None)]);
    wire.lose_acks = true;
    let mut client = Client::connect(wire).unwrap();
    let mut doc = Doc::default();
    assert!(client.key(typed("a"), private(), false, &mut doc));
    assert!(client.is_held());
    assert_eq!(doc.marked, "漢字");
    let sent = restores(
        &mut client,
        vec![on(2, response(1, 1, "かんじ", "かんじ", None))],
    );
    client.key(typed("y"), private(), false, &mut doc);
    let Some(request::Body::Restore(restore)) = &sent.borrow()[0].body else {
        panic!("expected a restore");
    };
    assert_eq!(restore.reading, "かんじ");
    assert!(doc.commits.is_empty());
}

#[test]
fn a_failure_with_nothing_displayed_leaves_the_key_to_the_application() {
    let mut client =
        Client::connect(Wire::new(1, vec![Err(io::ErrorKind::TimedOut.into())])).unwrap();
    let mut doc = Doc::default();
    assert!(!client.key(typed("a"), private(), false, &mut doc));
    assert!(!client.is_held());
    assert!(client.can_reconnect());
    assert!(doc.commits.is_empty());
}
