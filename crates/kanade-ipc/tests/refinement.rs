use kanade_ipc::{frontend::*, *};
use std::{cell::RefCell, collections::VecDeque, io, rc::Rc};

#[derive(Default)]
struct Calls {
    exchanges: Vec<Request>,
    notifications: Vec<Request>,
}
struct Script {
    responses: VecDeque<io::Result<Response>>,
    calls: Rc<RefCell<Calls>>,
}
impl Transport for Script {
    fn exchange(&mut self, request: &Request) -> io::Result<Response> {
        self.calls.borrow_mut().exchanges.push(request.clone());
        if matches!(request.body, Some(request::Body::Hello(_))) {
            return Ok(Response {
                body: Some(response::Body::Welcome(Welcome {
                    version: 2,
                    connection: 7,
                    ..Default::default()
                })),
            });
        }
        self.responses
            .pop_front()
            .unwrap_or_else(|| Err(io::ErrorKind::BrokenPipe.into()))
    }
    fn notify(&mut self, request: &Request) -> io::Result<()> {
        self.calls.borrow_mut().notifications.push(request.clone());
        Ok(())
    }
}
#[derive(Default)]
struct Doc {
    edits: usize,
    recovered: usize,
}
impl Document for Doc {
    fn apply(&mut self, _: &KeyReply) -> bool {
        self.edits += 1;
        true
    }
    fn finish_reading(&mut self, _: &str) -> bool {
        self.recovered += 1;
        true
    }
}
fn revision() -> Revision {
    Revision {
        connection: 7,
        context: 1,
        composition: 1,
    }
}
fn page(revision: u64) -> CandidatePage {
    CandidatePage {
        candidate_revision: revision,
        total: 3,
        selected: 0,
        font_size: 17.0,
        footer: "はる".into(),
        refinement_pending: revision == 1,
        items: ["春", "張る", "貼る"]
            .into_iter()
            .enumerate()
            .map(|(slot, text)| CandidateItem {
                handle: Some(CandidateHandle {
                    session_epoch: 7,
                    candidate_revision: revision,
                    slot: slot as u32,
                }),
                value: text.into(),
                reading: "はる".into(),
                source: CandidateSource::Mozc as i32,
                number: slot as u32 + 1,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}
fn update(page: Option<CandidatePage>, pending: bool, revision: Revision) -> Response {
    Response {
        body: Some(response::Body::Candidates(CandidateUpdate {
            revision: Some(revision),
            candidates: page,
            pending,
            ..Default::default()
        })),
    }
}
fn prepared(responses: Vec<io::Result<Response>>) -> (Client<Script>, Doc, Rc<RefCell<Calls>>) {
    let first = Response {
        body: Some(response::Body::Key(KeyReply {
            revision: Some(revision()),
            event_seq: 1,
            state_revision: 1,
            consumed: true,
            preedit: Some(Preedit {
                segments: vec![Segment {
                    reading: "はる".into(),
                    value: "春".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            candidates: Some(page(1)),
            key_policy: Some(KeyPolicySummary {
                composing: true,
                enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        })),
    };
    let calls = Rc::new(RefCell::new(Calls::default()));
    let mut client = Client::connect(Script {
        responses: std::iter::once(Ok(first)).chain(responses).collect(),
        calls: calls.clone(),
    })
    .unwrap();
    let mut doc = Doc::default();
    assert!(client.key(Key::default(), Context::default(), false, &mut doc));
    (client, doc, calls)
}

#[test]
fn candidate_updates_do_not_edit_acknowledge_or_consume_a_key_sequence() {
    let mut next = page(2);
    next.items.swap(1, 2);
    for (slot, item) in next.items.iter_mut().enumerate() {
        item.handle.as_mut().unwrap().slot = slot as u32;
        item.number = slot as u32 + 1;
    }
    let (mut client, mut doc, calls) =
        prepared(vec![Ok(update(Some(next.clone()), false, revision()))]);
    assert_eq!(client.poll_candidates(&page(1)).unwrap(), Some(next));
    assert_eq!(doc.edits, 1);
    assert_eq!(doc.recovered, 0);
    assert_eq!(calls.borrow().notifications.len(), 1);
    // A following event must still use sequence 2, irrespective of polling.
    client.key(Key::default(), Context::default(), false, &mut doc);
    let calls = calls.borrow();
    assert!(matches!(
        calls.exchanges[2].body,
        Some(request::Body::Poll(_))
    ));
    assert!(matches!(&calls.exchanges[3].body, Some(request::Body::Key(k)) if k.event_seq == 2));
}

#[test]
fn another_contexts_update_is_ignored() {
    let (mut client, doc, _) = prepared(vec![Ok(update(
        Some(page(2)),
        false,
        Revision {
            context: 2,
            ..revision()
        },
    ))]);
    assert!(client.poll_candidates(&page(1)).unwrap().is_none());
    assert!(client.is_connected());
    assert_eq!(doc.edits, 1);
}

#[test]
fn generated_or_replaced_candidates_are_rejected_without_a_document_edit() {
    let mut forged = page(2);
    forged.items[1].value = "別の語".into();
    let (mut client, doc, _) = prepared(vec![Ok(update(Some(forged), false, revision()))]);
    assert!(client.poll_candidates(&page(1)).is_err());
    assert!(!client.is_connected());
    assert_eq!(doc.edits, 1);
    assert_eq!(doc.recovered, 0);
}

#[test]
fn reranking_cannot_move_the_selected_candidate() {
    let mut next = page(2);
    next.items.swap(0, 1);
    for (slot, item) in next.items.iter_mut().enumerate() {
        item.handle.as_mut().unwrap().slot = slot as u32;
        item.number = slot as u32 + 1;
    }
    let (mut client, doc, _) = prepared(vec![Ok(update(Some(next), false, revision()))]);
    assert!(client.poll_candidates(&page(1)).is_err());
    assert!(!client.is_connected());
    assert_eq!(doc.edits, 1);
    assert_eq!(doc.recovered, 0);
}

#[test]
fn pending_and_finished_responses_control_polling_without_replacing_handles() {
    let (mut client, _, calls) = prepared(vec![
        Ok(update(None, true, revision())),
        Ok(update(None, false, revision())),
    ]);
    assert!(client.poll_candidates(&page(1)).unwrap().is_none());
    let finished = client.poll_candidates(&page(1)).unwrap().unwrap();
    assert!(!finished.refinement_pending);
    assert_eq!(finished.candidate_revision, 1);
    assert!(client.poll_candidates(&finished).unwrap().is_none());
    assert_eq!(calls.borrow().exchanges.len(), 4);
}

#[test]
fn status_updates_preserve_candidates_and_report_the_actual_outcome() {
    let current = page(1);
    assert_eq!(current.status_line(), "1 / 3　AI 評価中…");
    for (outcome, label) in [
        (RefinementOutcome::Evaluated, "AI 評価済み"),
        (RefinementOutcome::Unavailable, "AI 評価できず・通常候補"),
        (RefinementOutcome::TimedOut, "AI 時間切れ・通常候補"),
        (
            RefinementOutcome::SetupRequired,
            "AI は設定が必要・通常候補",
        ),
        (RefinementOutcome::Unspecified, "通常候補"),
    ] {
        let response = Response {
            body: Some(response::Body::Candidates(CandidateUpdate {
                revision: Some(revision()),
                refinement_outcome: outcome as i32,
                ..Default::default()
            })),
        };
        let (mut client, doc, _) = prepared(vec![Ok(response)]);
        let finished = client.poll_candidates(&current).unwrap().unwrap();
        assert_eq!(finished.items, current.items);
        assert_eq!(finished.status_line(), format!("1 / 3　{label}"));
        assert_eq!(doc.edits, 1);
    }
}

#[test]
fn password_bypass_stops_pending_candidate_polling() {
    let (mut client, mut doc, calls) = prepared(vec![]);
    assert!(!client.key(
        Key::default(),
        Context {
            privacy: Privacy::Password as i32,
            ..Default::default()
        },
        false,
        &mut doc,
    ));
    assert!(client.poll_candidates(&page(1)).unwrap().is_none());
    assert_eq!(calls.borrow().exchanges.len(), 2);
    assert_eq!(doc.edits, 1);
}
