// SPDX-License-Identifier: MIT
//! Engine contracts for background candidate reordering, with a scripted
//! conversion backend and a scripted reranker. No worker or model is involved.
use kanade_engine::{Engine, config::Config, dates::Date};
use kanade_ipc::*;
use kanade_llm::{Job, Poll, Rerank};
use kanade_mozc::{ConversionEngine, FocusCandidate, Result, SessionId, Snapshot};
use std::sync::{Arc, Mutex};

const APP: &str = "com.apple.TextEdit";
const CONTEXT: &str = "明日は市民ホールで";
const DIGIT_CONTEXT: &str = "昨日は2匹。";

#[derive(Default)]
struct Calls {
    keys: u32,
    selects: Vec<String>,
    submits: u32,
}
/// A minimal conversion backend: one composition, fixed candidates per segment.
#[derive(Default)]
struct Backend {
    probe: Option<Box<Backend>>,
    probe_binary: Option<std::path::PathBuf>,
    calls: Arc<Mutex<Calls>>,
    reading: String,
    segments: Vec<(String, Vec<&'static str>, usize)>,
    focused: usize,
    converting: bool,
}
fn table(reading: &str) -> Vec<(String, Vec<&'static str>, usize)> {
    let one = |reading: &str, values: Vec<&'static str>| vec![(reading.to_owned(), values, 0)];
    match reading {
        "かめらでとります" => vec![
            ("かめらで".into(), vec!["カメラで"], 0),
            (
                "とります".into(),
                vec!["取ります", "とります", "撮ります"],
                0,
            ),
        ],
        "こうえん" => one(reading, vec!["公園", "講演", "公演", "後援"]),
        "さんびき" => one(reading, vec!["３匹", "三匹"]),
        "こんげつまつ" => one(reading, vec!["今月末", "今月松", "根月末"]),
        "こうえんをきく" => vec![
            ("こうえんを".into(), vec!["公園を", "講演を", "公演を"], 0),
            ("きく".into(), vec!["聞く", "効く", "菊"], 0),
        ],
        "かいは、あめ。" => vec![
            ("かいは、".into(), vec!["会は、", "回は、"], 0),
            ("あめ。".into(), vec!["雨。", "飴。"], 0),
        ],
        "にいだ" => one(reading, vec!["二位だ", "二医だ"]),
        // A key event carries at most 32 bytes of text.
        "にせんいちねんのあめ" => vec![
            ("にせん".into(), vec!["二千"], 0),
            ("いちねんの".into(), vec!["一年の"], 0),
            ("あめ".into(), vec!["雨", "飴"], 0),
        ],
        other => one(other, vec!["候補一", "候補二"]),
    }
}
impl Backend {
    fn snapshot(&self) -> Snapshot {
        let segments: Vec<Segment> = if self.converting {
            self.segments
                .iter()
                .enumerate()
                .map(|(index, (reading, values, selected))| Segment {
                    reading: reading.clone(),
                    value: values[*selected].into(),
                    focused: index == self.focused,
                    value_utf16: utf16_len(values[*selected]),
                })
                .collect()
        } else if self.reading.is_empty() {
            vec![]
        } else {
            vec![Segment {
                reading: self.reading.clone(),
                value: self.reading.clone(),
                focused: true,
                value_utf16: utf16_len(&self.reading),
            }]
        };
        let candidates = if self.converting {
            let (reading, values, _) = &self.segments[self.focused];
            values
                .iter()
                .enumerate()
                .map(|(index, value)| FocusCandidate {
                    value: (*value).into(),
                    reading: reading.clone(),
                    annotation: String::new(),
                    transliteration: false,
                    history_deletable: index == 0,
                })
                .collect()
        } else {
            vec![]
        };
        Snapshot {
            consumed: true,
            preedit: Preedit {
                cursor_utf16: segments.iter().map(|s| s.value_utf16).sum(),
                segments,
            },
            candidates,
            focused_segment: self.focused,
            converting: self.converting,
            ..Default::default()
        }
    }
    fn commit(&mut self) -> Snapshot {
        let words: Vec<Word> = self
            .snapshot()
            .preedit
            .segments
            .iter()
            .map(|s| Word {
                reading: s.reading.clone(),
                value: s.value.clone(),
            })
            .collect();
        let commit = Commit {
            reading: words.iter().map(|w| w.reading.as_str()).collect(),
            value: words.iter().map(|w| w.value.as_str()).collect(),
            words,
            synthetic: false,
            transient: false,
        };
        self.clear();
        Snapshot {
            consumed: true,
            commit: Some(commit),
            ..Default::default()
        }
    }
    fn clear(&mut self) {
        self.reading.clear();
        self.segments.clear();
        self.focused = 0;
        self.converting = false;
    }
}
impl ConversionEngine for Backend {
    fn probe_binary(&self) -> Option<&std::path::Path> {
        self.probe_binary.as_deref()
    }
    fn add_history(&mut self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn create_session(&mut self) -> Result<SessionId> {
        if self.converting {
            self.probe = Some(Box::default());
            Ok(SessionId::scripted(2))
        } else {
            Ok(SessionId::scripted(1))
        }
    }
    fn restore(&mut self, id: SessionId, reading: &str) -> Result<Snapshot> {
        assert_eq!(id, SessionId::scripted(2));
        let probe = self.probe.as_mut().unwrap();
        probe.reading = reading.into();
        Ok(probe.snapshot())
    }
    fn delete_session(&mut self, id: SessionId) -> Result<()> {
        if id == SessionId::scripted(2) {
            self.probe = None;
        }
        Ok(())
    }
    fn set_privacy(&mut self, _: Privacy) -> Result<()> {
        Ok(())
    }
    fn key(&mut self, id: SessionId, key: &Key, preceding: &str) -> Result<Snapshot> {
        if id == SessionId::scripted(2) {
            assert!(matches!(preceding, "" | CONTEXT | DIGIT_CONTEXT));
            return self
                .probe
                .as_mut()
                .unwrap()
                .key(SessionId::scripted(1), key, preceding);
        }
        self.calls.lock().unwrap().keys += 1;
        match SpecialKey::try_from(key.special).unwrap() {
            SpecialKey::None if !self.converting => self.reading.push_str(&key.text),
            SpecialKey::Space if !self.converting && !self.reading.is_empty() => {
                self.segments = table(&self.reading);
                self.converting = true;
            }
            SpecialKey::Right if self.converting => {
                self.focused = (self.focused + 1).min(self.segments.len() - 1)
            }
            SpecialKey::Left if self.converting => self.focused = self.focused.saturating_sub(1),
            SpecialKey::Enter => return Ok(self.commit()),
            SpecialKey::Escape => self.converting = false,
            _ => {}
        }
        Ok(self.snapshot())
    }
    fn select(&mut self, _: SessionId, value: &str) -> Result<Snapshot> {
        self.calls.lock().unwrap().selects.push(value.into());
        let (_, values, selected) = &mut self.segments[self.focused];
        *selected = values
            .iter()
            .position(|v| *v == value)
            .ok_or(kanade_mozc::Error::StaleCandidate)?;
        Ok(self.snapshot())
    }
    fn delete_candidate_history(&mut self, _: SessionId, _: &str) -> Result<Snapshot> {
        Ok(self.snapshot())
    }
    fn submit(&mut self, _: SessionId) -> Result<Snapshot> {
        self.calls.lock().unwrap().submits += 1;
        Ok(self.commit())
    }
    fn reset(&mut self, _: SessionId) -> Result<()> {
        self.clear();
        Ok(())
    }
    fn undo_learning(&mut self, _: SessionId) -> Result<()> {
        Ok(())
    }
    fn sync(&mut self) -> Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct Script {
    jobs: Vec<Job>,
    cancelled: Vec<u64>,
    takes: u32,
    /// What `take` answers for every job.
    answer: Option<Poll>,
    /// Index of the candidate preferred by every job taken after the first taken one.
    later: Option<usize>,
    /// The first job taken once `later` is set. It receives `answer`.
    main: Option<u64>,
    /// Spellings that a job prefers when it offers one of them. A job that
    /// offers none prefers its first candidate.
    wanted: Vec<String>,
    refuse: bool,
}
#[derive(Clone, Default)]
struct Reranker(Arc<Mutex<Script>>);
impl Rerank for Reranker {
    fn submit(&self, job: Job) -> Option<u64> {
        let mut script = self.0.lock().unwrap();
        if script.refuse {
            return None;
        }
        script.jobs.push(job);
        Some(script.jobs.len() as u64)
    }
    fn take(&self, id: u64) -> Poll {
        let mut script = self.0.lock().unwrap();
        script.takes += 1;
        if !script.wanted.is_empty() {
            let candidates = &script.jobs[id as usize - 1].candidates;
            let preferred = candidates
                .iter()
                .position(|c| script.wanted.contains(c))
                .unwrap_or(0);
            let mut scores = vec![Some(-40.0); candidates.len()];
            scores[preferred] = Some(0.0);
            return Poll::Ready(scores);
        }
        if let Some(preferred) = script.later
            && *script.main.get_or_insert(id) != id
        {
            let count = script.jobs[id as usize - 1].candidates.len();
            let mut scores = vec![Some(-40.0); count];
            scores[preferred.min(count - 1)] = Some(0.0);
            return Poll::Ready(scores);
        }
        script.answer.clone().unwrap_or(Poll::Pending)
    }
    fn cancel(&self, id: u64) {
        self.0.lock().unwrap().cancelled.push(id);
    }
}
impl Reranker {
    fn jobs(&self) -> usize {
        self.0.lock().unwrap().jobs.len()
    }
    fn answer(&self, scores: &[Option<f32>]) {
        self.0.lock().unwrap().answer = Some(Poll::Ready(scores.to_vec()));
    }
    /// Every job clearly prefers one of these spellings, or else what is displayed.
    fn want(&self, spellings: &[&str]) {
        self.0.lock().unwrap().wanted = spellings.iter().map(|s| (*s).to_owned()).collect();
    }
    /// Jobs taken after the next one clearly prefer the candidate at `index`.
    fn later(&self, index: usize) {
        self.0.lock().unwrap().later = Some(index);
    }
    fn cancelled(&self) -> Vec<u64> {
        self.0.lock().unwrap().cancelled.clone()
    }
}

struct Fixture {
    engine: Engine<Backend>,
    calls: Arc<Mutex<Calls>>,
    llm: Reranker,
    connection: u64,
    revision: Revision,
    seq: u64,
    context: Context,
}
fn settings(llm: bool) -> Config {
    Config {
        llm,
        // No learning store is attached; this switch keeps normal contexts
        // eligible for the optional, separately consented reranker.
        learning: true,
        emoji_dictionary: false,
        calculated_dates: false,
        ..Config::default()
    }
}
fn fixture(config: Config) -> Fixture {
    fixture_with_backend(config, Backend::default())
}
fn fixture_with_backend(config: Config, backend: Backend) -> Fixture {
    let calls = backend.calls.clone();
    let mut engine = Engine::new(backend, None);
    engine.configure(config);
    let llm = Reranker::default();
    engine.set_reranker(Some(Box::new(llm.clone())));
    let welcome = engine
        .open(&Hello {
            min_version: OLDEST_VERSION,
            max_version: CURRENT_VERSION,
        })
        .unwrap();
    Fixture {
        engine,
        calls,
        llm,
        connection: welcome.connection,
        revision: Revision {
            connection: welcome.connection,
            context: 1,
            composition: 0,
        },
        seq: 0,
        context: Context {
            privacy: Privacy::Normal as i32,
            preceding_text: CONTEXT.into(),
            app: APP.into(),
            surrounding_available: true,
        },
    }
}
impl Fixture {
    fn request(&self, key: Key) -> Request {
        Request {
            body: Some(request::Body::Key(KeyRequest {
                revision: Some(self.revision),
                event_seq: self.seq + 1,
                key: Some(key),
                context: Some(self.context.clone()),
                passed_through: false,
            })),
        }
    }
    fn send(&mut self, request: Request) -> std::result::Result<KeyReply, failure::Code> {
        match self.engine.handle(self.connection, request)? {
            Some(Response {
                body: Some(response::Body::Key(reply)),
            }) => {
                self.revision = reply.revision.unwrap();
                self.seq = reply.event_seq;
                Ok(reply)
            }
            other => panic!("unexpected response: {}", other.is_some()),
        }
    }
    fn key(&mut self, key: Key) -> KeyReply {
        let request = self.request(key);
        self.send(request).unwrap()
    }
    fn text(&mut self, text: &str) -> KeyReply {
        self.key(Key {
            text: text.into(),
            ..Default::default()
        })
    }
    fn special(&mut self, special: SpecialKey) -> KeyReply {
        self.key(Key {
            special: special as i32,
            ..Default::default()
        })
    }
    fn ack(&mut self, reply: &KeyReply) {
        let response = self
            .engine
            .handle(
                self.connection,
                Request {
                    body: Some(request::Body::Ack(Ack {
                        revision: reply.revision,
                        event_seq: reply.event_seq,
                        applied: true,
                    })),
                },
            )
            .unwrap();
        assert!(response.is_none());
    }
    /// Types a reading, acknowledges it, starts conversion and acknowledges that.
    fn convert(&mut self, reading: &str) -> KeyReply {
        let typed = self.text(reading);
        self.ack(&typed);
        let converted = self.special(SpecialKey::Space);
        self.ack(&converted);
        converted
    }
    fn poll_at(&mut self, revision: Revision, candidate_revision: u64) -> CandidateUpdate {
        match self
            .engine
            .handle(
                self.connection,
                Request {
                    body: Some(request::Body::Poll(CandidatePoll {
                        preview_event_seq: None,
                        revision: Some(revision),
                        candidate_revision,
                    })),
                },
            )
            .unwrap()
        {
            Some(Response {
                body: Some(response::Body::Candidates(update)),
            }) => {
                assert_eq!(update.revision, Some(revision));
                update
            }
            _ => panic!("a poll is always answered with a candidate update"),
        }
    }
    fn poll(&mut self, page: &CandidatePage) -> CandidateUpdate {
        self.poll_at(self.revision, page.candidate_revision)
    }
}
fn values(page: &CandidatePage) -> Vec<&str> {
    page.items.iter().map(|item| item.value.as_str()).collect()
}
fn page(reply: &KeyReply) -> &CandidatePage {
    reply.candidates.as_ref().unwrap()
}

#[test]
fn nothing_is_queued_when_disabled_or_outside_a_normal_safe_context() {
    // Disabled by default: no job, no flag, and the list is Mozc's.
    let mut off = fixture(settings(false));
    let reply = off.convert("こうえん");
    assert_eq!(values(page(&reply)), ["公園", "講演", "公演", "後援"]);
    assert!(!page(&reply).refinement_pending);
    assert_eq!(off.llm.jobs(), 0);
    assert!(!Config::default().llm);

    type Edit = fn(&mut Fixture);
    let cases: [(&str, Edit); 10] = [
        ("private", |f| f.context.privacy = Privacy::Private as i32),
        ("unknown", |f| f.context.privacy = Privacy::Unknown as i32),
        ("excluded application", |f| {
            f.engine.configure(Config {
                learning_excluded_apps: vec![APP.into()],
                ..settings(true)
            });
        }),
        ("unknown application", |f| f.context.app.clear()),
        ("missing surrounding text", |f| {
            f.context.surrounding_available = false
        }),
        ("empty context", |f| f.context.preceding_text.clear()),
        ("address in context", |f| {
            f.context.preceding_text = "宛先は you@example.com で".into()
        }),
        ("link in context", |f| {
            f.context.preceding_text = "https://example.com/ を".into()
        }),
        ("credential-like context", |f| {
            f.context.preceding_text = format!("鍵 {}", "k".repeat(40))
        }),
        ("refused by the service", |f| {
            f.llm.0.lock().unwrap().refuse = true
        }),
    ];
    for (name, edit) in cases {
        let mut f = fixture(settings(true));
        edit(&mut f);
        let reply = f.convert("こうえん");
        assert_eq!(f.llm.jobs(), 0, "{name}");
        assert!(!page(&reply).refinement_pending, "{name}");
        let expected = if name == "refused by the service" {
            RefinementOutcome::Unavailable
        } else {
            RefinementOutcome::Unspecified
        };
        assert_eq!(page(&reply).refinement_outcome(), expected, "{name}");
        // A poll in such a state is answered cheaply with "nothing".
        let update = f.poll(page(&reply));
        assert!(update.candidates.is_none() && !update.pending, "{name}");
        assert_eq!(f.llm.0.lock().unwrap().takes, 0, "{name}");
    }

    // Password input is refused before any conversion or job.
    let mut f = fixture(settings(true));
    f.context.privacy = Privacy::Password as i32;
    let request = f.request(Key {
        text: "こうえん".into(),
        ..Default::default()
    });
    assert_eq!(f.send(request).err(), Some(failure::Code::Privacy));
    assert_eq!(f.llm.jobs(), 0);

    // A reading that is too short is not sent either.
    let mut f = fixture(settings(true));
    f.convert("こ");
    assert_eq!(f.llm.jobs(), 0);
}

#[test]
fn excluding_an_app_cancels_its_pending_refinement() {
    let mut f = fixture(settings(true));
    let reply = f.convert("こうえん");
    assert!(page(&reply).refinement_pending);
    assert_eq!(f.llm.jobs(), 1);
    f.engine.configure(Config {
        learning_excluded_apps: vec![APP.into()],
        ..settings(true)
    });
    assert_eq!(f.llm.cancelled().len(), 1);
    f.llm.answer(&[Some(-4.), Some(-1.), Some(-2.), Some(-3.)]);
    let update = f.poll(page(&reply));
    assert!(!update.pending && update.candidates.is_none());
}

#[test]
fn a_job_carries_only_the_bounded_window_reading_and_displayed_choices() {
    let mut f = fixture(settings(true));
    f.context.preceding_text = format!("{}。{CONTEXT}", "前".repeat(200));
    let reply = f.convert("こうえん");
    assert!(page(&reply).refinement_pending);
    let script = f.llm.0.lock().unwrap();
    assert_eq!(script.jobs.len(), 1);
    let job = &script.jobs[0];
    assert!(job.valid());
    assert_eq!(
        job.context.chars().count(),
        kanade_llm::limits::CONTEXT_CHARS
    );
    assert!(job.context.ends_with(CONTEXT));
    assert_eq!(job.reading, "コウエン");
    assert_eq!((job.prefix.as_str(), job.suffix.as_str()), ("", ""));
    assert_eq!(job.candidates, ["公園", "講演", "公演", "後援"]);
}

#[test]
fn an_update_reorders_unselected_mozc_choices_and_keeps_the_displayed_surface() {
    let mut f = fixture(settings(true));
    let reply = f.convert("こうえん");
    let before = page(&reply).clone();
    let keys = f.calls.lock().unwrap().keys;

    // Still running: no update, ask again.
    let update = f.poll(&before);
    assert!(update.candidates.is_none() && update.pending);

    // The model prefers 後援, then 公演, 講演; 公園 is displayed and selected.
    f.llm
        .answer(&[Some(-6.0), Some(-5.0), Some(-3.0), Some(-1.0)]);
    let update = f.poll(&before);
    assert!(!update.pending);
    let after = update.candidates.unwrap();
    assert_eq!(values(&after), ["公園", "後援", "公演", "講演"]);
    assert_eq!((after.selected, after.start, after.total), (0, 0, 4));
    assert!(!after.refinement_pending);
    assert_eq!(after.refinement_outcome(), RefinementOutcome::Evaluated);
    assert!(after.candidate_revision > before.candidate_revision);
    assert!(after.items.iter().all(|item| {
        item.source == CandidateSource::Mozc as i32
            && item.handle.unwrap().candidate_revision == after.candidate_revision
    }));
    assert_eq!(after.items[1].annotation, "文脈");
    assert!(after.items[2].annotation.is_empty());
    // A poll never reaches the conversion backend and changes no revision.
    assert_eq!(f.calls.lock().unwrap().keys, keys);
    assert!(f.calls.lock().unwrap().selects.is_empty());

    // Enter commits exactly what the preedit showed before the update.
    let committed = f.special(SpecialKey::Enter);
    let commit = committed.commit.clone().unwrap();
    assert_eq!(
        (commit.reading.as_str(), commit.value.as_str()),
        ("こうえん", "公園")
    );
    assert!(!commit.synthetic && !commit.transient);
    assert_eq!(f.calls.lock().unwrap().submits, 1);
    f.ack(&committed);
}

#[test]
fn a_repeated_poll_or_key_returns_the_applied_update_without_new_work() {
    let mut f = fixture(settings(true));
    let typed = f.text("こうえん");
    f.ack(&typed);
    let convert = f.request(Key {
        special: SpecialKey::Space as i32,
        ..Default::default()
    });
    let reply = f.send(convert.clone()).unwrap();
    f.ack(&reply);
    let before = page(&reply).clone();
    f.llm
        .answer(&[Some(-6.0), Some(-5.0), Some(-3.0), Some(-1.0)]);
    let first = f.poll(&before).candidates.unwrap();
    let takes = f.llm.0.lock().unwrap().takes;
    let keys = f.calls.lock().unwrap().keys;

    // The response was lost: the same poll gets the same page, not a new sort.
    let again = f.poll(&before).candidates.unwrap();
    assert_eq!(again, first);
    // Polling with the new page reports that nothing else is coming.
    let done = f.poll(&first);
    assert!(done.candidates.is_none() && !done.pending);
    assert_eq!(f.llm.0.lock().unwrap().takes, takes);

    // A retried key event is answered from the cache with the displayed list.
    let retried = match f.engine.handle(f.connection, convert).unwrap() {
        Some(Response {
            body: Some(response::Body::Key(reply)),
        }) => reply,
        _ => panic!("cached reply expected"),
    };
    assert_eq!(retried.event_seq, reply.event_seq);
    assert_eq!(retried.candidates.as_ref(), Some(&first));
    assert_eq!(f.llm.jobs(), 1);
    assert_eq!(f.calls.lock().unwrap().keys, keys);
}

#[test]
fn unacknowledged_stale_and_malformed_states_get_no_update() {
    let mut f = fixture(settings(true));
    let typed = f.text("こうえん");
    f.ack(&typed);
    let reply = f.special(SpecialKey::Space);
    let before = page(&reply).clone();
    f.llm
        .answer(&[Some(-6.0), Some(-5.0), Some(-3.0), Some(-1.0)]);

    // Before the acknowledgement the result stays untouched.
    let early = f.poll(&before);
    assert!(early.candidates.is_none() && early.pending);
    assert_eq!(f.llm.0.lock().unwrap().takes, 0);
    f.ack(&reply);

    // Another context, composition or candidate revision is simply stale.
    let mut other = f.revision;
    other.composition += 1;
    assert!(
        f.poll_at(other, before.candidate_revision)
            .candidates
            .is_none()
    );
    other = f.revision;
    other.context += 1;
    assert!(
        f.poll_at(other, before.candidate_revision)
            .candidates
            .is_none()
    );
    let stale = f.poll_at(f.revision, before.candidate_revision + 1);
    assert!(stale.candidates.is_none() && !stale.pending);
    assert_eq!(f.llm.0.lock().unwrap().takes, 0);

    // A poll without a revision is a protocol error.
    let malformed = f.engine.handle(
        f.connection,
        Request {
            body: Some(request::Body::Poll(CandidatePoll::default())),
        },
    );
    assert_eq!(malformed.err(), Some(failure::Code::Invalid));

    // The valid poll still works afterwards.
    assert!(f.poll(&before).candidates.is_some());
}

#[test]
fn unusable_scores_leave_the_list_alone() {
    for scores in [
        vec![Some(-1.0), Some(-2.0)],
        vec![Some(-1.0), Some(f32::NAN), Some(-2.0), Some(-3.0)],
        vec![Some(-1.0), Some(f32::NEG_INFINITY), Some(-2.0), Some(-3.0)],
        vec![None, None, None, None],
        vec![Some(-9.0), None, Some(-1.0), None],
        // Already in the model's order.
        vec![Some(-9.0), Some(-1.0), Some(-2.0), Some(-3.0)],
    ] {
        let mut f = fixture(settings(true));
        let reply = f.convert("こうえん");
        f.llm.answer(&scores);
        let update = f.poll(page(&reply));
        assert!(update.candidates.is_none() && !update.pending, "{scores:?}");
        let expected = if scores.len() == 4
            && scores.iter().flatten().all(|s| s.is_finite())
            && scores.iter().any(Option::is_some)
        {
            RefinementOutcome::Evaluated
        } else {
            RefinementOutcome::Unavailable
        };
        assert_eq!(update.refinement_outcome(), expected, "{scores:?}");
        assert_eq!(f.poll(page(&reply)), update);
        // The old handles stay valid because nothing moved.
        let enter = f.special(SpecialKey::Enter);
        assert_eq!(enter.commit.unwrap().value, "公園");
    }
    let mut f = fixture(settings(true));
    let reply = f.convert("こうえん");
    f.llm.0.lock().unwrap().answer = Some(Poll::Gone);
    let failed = f.poll(page(&reply));
    assert_eq!(failed.refinement_outcome(), RefinementOutcome::Unavailable);
    assert_eq!(f.special(SpecialKey::Enter).commit.unwrap().value, "公園");
    // Unscored choices keep their slots while the scored ones are ordered.
    let mut f = fixture(settings(true));
    let reply = f.convert("こうえん");
    f.llm.answer(&[Some(-1.0), Some(-5.0), None, Some(-2.0)]);
    let after = f.poll(page(&reply)).candidates.unwrap();
    assert_eq!(values(&after), ["公園", "後援", "公演", "講演"]);
    assert!(after.items.iter().all(|item| item.annotation.is_empty()));
}

#[test]
fn events_and_setting_changes_cancel_the_job_and_reject_its_result() {
    type Event = fn(&mut Fixture, &KeyReply);
    let events: [(&str, Event); 6] = [
        ("next candidate", |f, _| {
            let next = f.special(SpecialKey::Space);
            assert_eq!(page(&next).selected, 1);
            // A manual choice freezes the list: no second job.
            assert!(!page(&next).refinement_pending);
        }),
        ("click", |f, reply| {
            let handle = page(reply).items[2].handle;
            let request = Request {
                body: Some(request::Body::Select(SelectRequest {
                    revision: Some(f.revision),
                    event_seq: f.seq + 1,
                    handle,
                    submit: false,
                })),
            };
            let selected = f.send(request).unwrap();
            assert_eq!(page(&selected).selected, 2);
            assert!(!page(&selected).refinement_pending);
        }),
        ("cancel conversion", |f, _| {
            f.special(SpecialKey::Escape);
        }),
        ("focus change", |f, _| {
            let context = f.revision.context + 1;
            f.engine
                .handle(
                    f.connection,
                    Request {
                        body: Some(request::Body::Reset(Reset { context })),
                    },
                )
                .unwrap();
        }),
        ("setting switched off", |f, _| {
            f.engine.configure(settings(false))
        }),
        ("reranker replaced", |f, _| f.engine.set_reranker(None)),
    ];
    for (name, event) in events {
        let mut f = fixture(settings(true));
        let reply = f.convert("こうえん");
        let revision = f.revision;
        assert_eq!(f.llm.jobs(), 1, "{name}");
        event(&mut f, &reply);
        assert_eq!(f.llm.cancelled(), [1], "{name}");
        // The finished result of the cancelled job is never applied.
        f.llm
            .answer(&[Some(-6.0), Some(-5.0), Some(-3.0), Some(-1.0)]);
        let update = f.poll_at(revision, page(&reply).candidate_revision);
        assert!(update.candidates.is_none() && !update.pending, "{name}");
        assert_eq!(f.llm.jobs(), 1, "{name}");
        assert_eq!(f.llm.0.lock().unwrap().takes, 0, "{name}");
    }
    // Closing the connection cancels as well.
    let mut f = fixture(settings(true));
    f.convert("こうえん");
    f.engine.close(f.connection);
    assert_eq!(f.llm.cancelled(), [1]);
}

#[test]
fn a_result_older_than_the_display_deadline_is_dropped() {
    let mut f = fixture(settings(true));
    let reply = f.convert("こうえん");
    f.llm
        .answer(&[Some(-6.0), Some(-5.0), Some(-3.0), Some(-1.0)]);
    std::thread::sleep(std::time::Duration::from_millis(
        kanade_llm::limits::RESULT_TTL_MS + 50,
    ));
    let update = f.poll(page(&reply));
    assert!(update.candidates.is_none() && !update.pending);
    assert_eq!(update.refinement_outcome(), RefinementOutcome::TimedOut);
    assert_eq!(f.poll(page(&reply)), update);
    assert_eq!(f.llm.cancelled(), [1]);
    let enter = f.special(SpecialKey::Enter);
    assert_eq!(enter.commit.unwrap().value, "公園");
}

#[test]
fn navigation_after_an_update_keeps_the_order_and_selects_real_candidates() {
    let mut f = fixture(settings(true));
    let reply = f.convert("こうえん");
    f.llm
        .answer(&[Some(-6.0), Some(-5.0), Some(-3.0), Some(-1.0)]);
    let updated = f.poll(page(&reply)).candidates.unwrap();
    assert_eq!(values(&updated), ["公園", "後援", "公演", "講演"]);

    // The handle from before the update names another slot now: refused.
    let stale = Request {
        body: Some(request::Body::Select(SelectRequest {
            revision: Some(f.revision),
            event_seq: f.seq + 1,
            handle: page(&reply).items[1].handle,
            submit: true,
        })),
    };
    assert_eq!(
        f.engine.handle(f.connection, stale).err(),
        Some(failure::Code::Stale)
    );

    // Space moves to the model's first choice, through the real backend.
    let next = f.special(SpecialKey::Space);
    assert_eq!(values(page(&next)), ["公園", "後援", "公演", "講演"]);
    assert_eq!(page(&next).selected, 1);
    assert_eq!(page(&next).items[1].annotation, "文脈");
    assert_eq!(
        page(&next).refinement_outcome(),
        RefinementOutcome::Evaluated
    );
    assert_eq!(f.calls.lock().unwrap().selects, ["後援"]);
    assert!(!page(&next).refinement_pending);
    assert_eq!(f.llm.jobs(), 1);
    f.ack(&next);

    // A digit follows the displayed numbering.
    let digit = f.text("3");
    assert_eq!(digit.commit.clone().unwrap().value, "公演");
    assert_eq!(f.calls.lock().unwrap().selects, ["後援", "公演"]);
    f.ack(&digit);

    // A click on a new handle selects that surface and commits it.
    let reply = f.convert("こうえん");
    let updated = f.poll(page(&reply)).candidates.unwrap();
    let clicked = f
        .send(Request {
            body: Some(request::Body::Select(SelectRequest {
                revision: Some(f.revision),
                event_seq: f.seq + 1,
                handle: updated.items[3].handle,
                submit: true,
            })),
        })
        .unwrap();
    assert_eq!(clicked.commit.unwrap().value, "講演");
}

#[test]
fn choices_from_other_sources_keep_their_slots() {
    let mut config = settings(true);
    config.calculated_dates = true;
    let mut f = fixture(config);
    f.engine.set_date(Some(Date::new(2026, 11, 15).unwrap()));
    let reply = f.convert("こんげつまつ");
    let before = page(&reply).clone();
    let dates: Vec<usize> = before
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.source == CandidateSource::Date as i32)
        .map(|(slot, _)| slot)
        .collect();
    assert!(!dates.is_empty());
    // Only Mozc surfaces are sent.
    assert_eq!(
        f.llm.0.lock().unwrap().jobs[0].candidates,
        ["今月末", "今月松", "根月末"]
    );
    f.llm.answer(&[Some(-1.0), Some(-9.0), Some(-2.0)]);
    let after = f.poll(&before).candidates.unwrap();
    for slot in &dates {
        assert_eq!(after.items[*slot], {
            let mut item = before.items[*slot].clone();
            item.handle.as_mut().unwrap().candidate_revision = after.candidate_revision;
            item
        });
    }
    let mozc = |page: &CandidatePage| -> Vec<String> {
        page.items
            .iter()
            .filter(|item| item.source == CandidateSource::Mozc as i32)
            .map(|item| item.value.clone())
            .collect()
    };
    assert_eq!(mozc(&before), ["今月末", "今月松", "根月末"]);
    assert_eq!(mozc(&after), ["今月末", "根月末", "今月松"]);
    assert_eq!(after.selected, before.selected);
}

#[test]
fn a_focused_segment_is_scored_inside_the_whole_displayed_sentence() {
    let mut f = fixture(settings(true));
    let reply = f.convert("こうえんをきく");
    {
        let script = f.llm.0.lock().unwrap();
        let job = &script.jobs[0];
        assert_eq!(job.reading, "コウエンヲキク");
        assert_eq!((job.prefix.as_str(), job.suffix.as_str()), ("", "聞く"));
        assert_eq!(job.candidates, ["公園を", "講演を", "公演を"]);
    }
    f.llm.answer(&[Some(-4.0), Some(-3.0), Some(-1.0)]);
    let after = f.poll(page(&reply)).candidates.unwrap();
    assert_eq!(values(&after), ["公園を", "公演を", "講演を"]);

    // Moving the focus builds another list and may start another job.
    let moved = f.special(SpecialKey::Right);
    assert_eq!(values(page(&moved)), ["聞く", "効く", "菊"]);
    assert!(page(&moved).refinement_pending);
    assert_eq!(f.llm.cancelled(), Vec::<u64>::new());
    let script = f.llm.0.lock().unwrap();
    assert_eq!(script.jobs.len(), 2);
    assert_eq!(
        (
            script.jobs[1].prefix.as_str(),
            script.jobs[1].suffix.as_str()
        ),
        ("公園を", "")
    );
    assert_eq!(script.jobs[1].candidates, ["聞く", "効く", "菊"]);
}

#[test]
fn commits_and_acknowledgements_match_the_disabled_engine() {
    let run = |llm: bool| {
        let mut f = fixture(settings(llm));
        let reply = f.convert("こうえん");
        if llm {
            f.llm
                .answer(&[Some(-6.0), Some(-5.0), Some(-3.0), Some(-1.0)]);
            assert!(f.poll(page(&reply)).candidates.is_some());
        }
        let committed = f.special(SpecialKey::Enter);
        // The commit waits for its acknowledgement exactly as before.
        let next = f.request(Key {
            text: "あ".into(),
            ..Default::default()
        });
        assert_eq!(
            f.engine.handle(f.connection, next).err(),
            Some(failure::Code::Sequence)
        );
        f.ack(&committed);
        (committed.commit, committed.preedit, committed.key_policy)
    };
    assert_eq!(run(true), run(false));
}

/// End to end with the real worker and model, still without Mozc:
///
/// ```sh
/// cargo build -p kanade-llm --features llama --bin kanade-llm-worker
/// KANADE_LLM_WORKER="$PWD/target/debug/kanade-llm-worker" KANADE_LLM_MODEL=/path/to/model.gguf \
///   cargo test -p kanade-engine --test refine -- --ignored
/// ```
#[test]
#[ignore = "needs KANADE_LLM_WORKER and KANADE_LLM_MODEL"]
fn the_real_model_reorders_candidates_through_the_engine() {
    use kanade_llm::manager::{ImportRequest, JobPhase, StartResult};
    use std::time::{Duration, Instant};
    let worker = std::path::absolute(std::env::var_os("KANADE_LLM_WORKER").unwrap()).unwrap();
    let model = std::path::absolute(std::env::var_os("KANADE_LLM_MODEL").unwrap()).unwrap();
    let data = tempfile::tempdir().unwrap();
    let manager = kanade_llm::Manager::open(data.path().join("llm"), worker, true);
    let entry = &kanade_llm::profile::CATALOG[0];
    let started = manager.import(ImportRequest {
        path: model,
        model: entry.id.into(),
        license: entry.license.into(),
        terms_sha256: kanade_llm::profile::terms_sha256(entry),
        accept_license: true,
        accept_local_processing: true,
    });
    assert_eq!(started.result, StartResult::Started);
    let deadline = Instant::now() + Duration::from_secs(120);
    while !manager
        .status()
        .job
        .is_some_and(|job| matches!(job.phase, JobPhase::Done | JobPhase::Failed))
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
    let status = manager.status();
    assert!(status.ready, "{:?} {:?}", status.reason, status.job);

    let mut f = fixture(settings(true));
    f.engine.set_reranker(Some(Box::new(manager.service())));
    f.context.preceding_text = "劇団の東京".into();
    // The first requests arrive while the worker is still loading the model
    // and expire; a later one is answered inside the display deadline.
    let deadline = Instant::now() + Duration::from_secs(60);
    let (updated, waited) = 'converted: loop {
        assert!(Instant::now() < deadline, "no update arrived");
        let reply = f.convert("こうえん");
        let submitted = Instant::now();
        assert_eq!(values(page(&reply)), ["公園", "講演", "公演", "後援"]);
        if page(&reply).refinement_pending {
            loop {
                let update = f.poll(page(&reply));
                if let Some(updated) = update.candidates {
                    break 'converted (updated, submitted.elapsed());
                }
                if !update.pending {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let cancelled = f.special(SpecialKey::Escape);
        f.ack(&cancelled);
        let cleared = f.special(SpecialKey::Enter);
        f.ack(&cleared);
        std::thread::sleep(Duration::from_millis(100));
    };
    println!("update after {} ms", waited.as_millis());
    assert!(waited < Duration::from_millis(kanade_llm::limits::RESULT_TTL_MS));
    // The displayed surface stays; the stage sense comes next.
    assert_eq!(updated.selected, 0);
    assert_eq!(updated.items[0].value, "公園");
    assert_eq!(updated.items[1].value, "公演");
    assert_eq!(updated.items[1].annotation, "文脈");
    let next = f.special(SpecialKey::Space);
    f.ack(&next);
    let committed = f.special(SpecialKey::Enter);
    assert_eq!(committed.commit.clone().unwrap().value, "公演");
    f.ack(&committed);

    // With the worker warm, an update arrives within a few polls.
    f.context.preceding_text = "このイベントは市の".into();
    let reply = f.convert("こうえん");
    let submitted = Instant::now();
    let updated = loop {
        let update = f.poll(page(&reply));
        if let Some(updated) = update.candidates {
            break updated;
        }
        assert!(update.pending, "the warm request was dropped");
        std::thread::sleep(Duration::from_millis(2));
    };
    let waited = submitted.elapsed();
    println!("warm update after {} ms", waited.as_millis());
    assert!(waited < Duration::from_millis(300));
    assert_eq!(updated.items[1].value, "後援");
}

#[test]
fn unavailable_search_leaves_the_displayed_conversion_committable() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Backend {
        probe_binary: Some(temp.path().join("absent-worker")),
        ..Default::default()
    };
    let mut f = fixture_with_backend(settings(true), backend);
    f.context.preceding_text.clear();
    let initial = f.convert("かめらでとります");
    let initial_surface = surface(initial.preedit.as_ref().unwrap());
    let page = page(&initial);
    assert!(page.refinement_pending);
    let request = Request {
        body: Some(request::Body::Poll(CandidatePoll {
            revision: Some(f.revision),
            candidate_revision: page.candidate_revision,
            preview_event_seq: Some(f.seq + 1),
        })),
    };
    let start = std::time::Instant::now();
    loop {
        let response = f
            .engine
            .handle(f.connection, request.clone())
            .unwrap()
            .unwrap();
        let Some(response::Body::Candidates(update)) = response.body else {
            panic!("unexpected preview")
        };
        assert!(update.candidates.is_none());
        if !update.pending {
            assert_eq!(
                update.refinement_outcome,
                RefinementOutcome::Unavailable as i32
            );
            break;
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(f.engine.worker_healthy());
    assert_eq!(
        f.special(SpecialKey::Enter).commit.unwrap().value,
        initial_surface
    );
}

#[test]
fn sentence_preview_is_acknowledged_once_and_plain_enter_commits_it() {
    let mut f = fixture(settings(true));
    f.context.preceding_text.clear();
    let initial = f.convert("かめらでとります");
    let page = page(&initial);
    assert!(page.refinement_pending);
    let request = Request {
        body: Some(request::Body::Poll(CandidatePoll {
            revision: Some(f.revision),
            candidate_revision: page.candidate_revision,
            preview_event_seq: Some(f.seq + 1),
        })),
    };
    f.llm.answer(&[Some(-4.0), Some(-0.2)]);
    let response = f
        .engine
        .handle(f.connection, request.clone())
        .unwrap()
        .unwrap();
    let Some(response::Body::Key(preview)) = response.body else {
        panic!("expected a preview")
    };
    assert_eq!(
        surface(preview.preedit.as_ref().unwrap()),
        "カメラで撮ります"
    );
    assert!(preview.commit.is_none());
    let retry = f.engine.handle(f.connection, request).unwrap().unwrap();
    assert_eq!(retry.body, Some(response::Body::Key(preview.clone())));
    assert_eq!(f.llm.jobs(), 1);
    f.seq = preview.event_seq;
    f.revision = preview.revision.unwrap();
    f.ack(&preview);
    let committed = f.special(SpecialKey::Enter);
    let commit = committed.commit.as_ref().unwrap();
    assert_eq!(commit.value, "カメラで撮ります");
    assert!(commit.transient);
}

#[test]
fn manual_navigation_supersedes_a_sentence_poll() {
    let mut f = fixture(settings(true));
    f.context.preceding_text.clear();
    let initial = f.convert("かめらでとります");
    let old = CandidatePoll {
        revision: Some(f.revision),
        candidate_revision: page(&initial).candidate_revision,
        preview_event_seq: Some(f.seq + 1),
    };
    let request = Request {
        body: Some(request::Body::Poll(old)),
    };
    f.engine.handle(f.connection, request.clone()).unwrap();
    let navigation = f.special(SpecialKey::Right);
    f.ack(&navigation);
    let response = f.engine.handle(f.connection, request).unwrap().unwrap();
    assert!(matches!(
        response.body,
        Some(response::Body::Candidates(CandidateUpdate {
            pending: false,
            candidates: None,
            ..
        }))
    ));
}

#[test]
fn contextual_alternatives_need_a_clear_advantage_over_the_default() {
    for (default_score, changes) in [(-8.0, true), (-5.5, false)] {
        let mut f = fixture(settings(true));
        let initial = f.convert("こうえんをきく");
        let request = Request {
            body: Some(request::Body::Poll(CandidatePoll {
                revision: Some(f.revision),
                candidate_revision: page(&initial).candidate_revision,
                preview_event_seq: Some(f.seq + 1),
            })),
        };
        f.llm.answer(&[
            Some(default_score),
            Some(0.),
            Some(-10.),
            Some(-11.),
            Some(-12.),
            None,
            None,
            None,
        ]);
        let response = f.engine.handle(f.connection, request).unwrap().unwrap();
        let script = f.llm.0.lock().unwrap();
        let job = script.jobs.last().unwrap();
        assert!(!job.context.is_empty());
        assert!(job.prefix.is_empty());
        assert_eq!(
            job.candidates,
            [
                "公園を聞く",
                "講演を聞く",
                "公園を効く",
                "公演を聞く",
                "公園を菊",
                "講演を効く",
                "公演を効く",
                "講演を菊"
            ]
        );
        if changes {
            let Some(response::Body::Key(reply)) = response.body else {
                panic!("expected preview");
            };
            assert_eq!(surface(reply.preedit.as_ref().unwrap()), "講演を聞く");
            assert!(reply.commit.is_none());
        } else {
            assert!(matches!(response.body, Some(response::Body::Candidates(_))));
        }
    }
}

#[test]
fn nearby_joint_spellings_keep_the_displayed_default() {
    for (other_joint, use_winner) in [(15.5, false), (-10.0, true)] {
        let mut f = fixture(settings(true));
        let initial = f.convert("こうえんをきく");
        let request = Request {
            body: Some(request::Body::Poll(CandidatePoll {
                revision: Some(f.revision),
                candidate_revision: page(&initial).candidate_revision,
                preview_event_seq: Some(f.seq + 1),
            })),
        };
        f.llm.answer(&[
            Some(0.0),
            Some(-1.0),
            Some(-2.0),
            Some(-3.0),
            Some(-4.0),
            Some(16.0),
            Some(other_joint),
            None,
        ]);
        let response = f.engine.handle(f.connection, request).unwrap().unwrap();
        if use_winner {
            let Some(response::Body::Key(reply)) = response.body else {
                panic!("expected preview")
            };
            assert_eq!(surface(reply.preedit.as_ref().unwrap()), "講演を効く");
        } else {
            assert!(matches!(response.body, Some(response::Body::Candidates(_))));
        }
    }
}

#[test]
fn an_accepted_spelling_is_scored_again_for_a_second_change() {
    let mut f = fixture(settings(true));
    // A model registered with two accepted spellings per window.
    f.engine.set_preview(kanade_llm::profile::Preview {
        rounds: 2,
        ..kanade_llm::profile::CATALOG[0].preview
    });
    let initial = f.convert("こうえんをきく");
    let request = Request {
        body: Some(request::Body::Poll(CandidatePoll {
            revision: Some(f.revision),
            candidate_revision: page(&initial).candidate_revision,
            preview_event_seq: Some(f.seq + 1),
        })),
    };
    // The first job accepts 講演 for the first segment.
    f.llm.answer(&[
        Some(-8.0),
        Some(0.),
        Some(-10.),
        Some(-11.),
        Some(-12.),
        None,
        None,
        None,
    ]);
    // The second job starts from that spelling and accepts another word for the next segment.
    f.llm.later(2);
    let response = f.engine.handle(f.connection, request).unwrap().unwrap();
    let script = f.llm.0.lock().unwrap();
    // The focused segment's own job, then two looks at the whole text.
    assert_eq!(script.jobs.len(), 3);
    let second = &script.jobs[2];
    assert_eq!(second.candidates[0], "講演を聞く");
    assert_eq!(second.context, script.jobs[1].context);
    assert_eq!(second.reading, script.jobs[1].reading);
    let Some(response::Body::Key(reply)) = response.body else {
        panic!("expected preview");
    };
    // Both accepted spellings are shown at once, and no third job follows.
    assert_eq!(second.candidates[1], "公園を聞く");
    assert_eq!(second.candidates[2], "講演を効く");
    assert_eq!(surface(reply.preedit.as_ref().unwrap()), "講演を効く");
}

/// Polls once for the sentence preview of a conversion.
fn preview_poll(f: &mut Fixture, initial: &KeyReply) -> response::Body {
    let request = Request {
        body: Some(request::Body::Poll(CandidatePoll {
            revision: Some(f.revision),
            candidate_revision: page(initial).candidate_revision,
            preview_event_seq: Some(f.seq + 1),
        })),
    };
    let response = f.engine.handle(f.connection, request).unwrap().unwrap();
    response.body.unwrap()
}

/// Polls once for the sentence preview and returns the text it displayed, if it changed.
fn previewed(f: &mut Fixture, initial: &KeyReply) -> Option<String> {
    match preview_poll(f, initial) {
        response::Body::Key(reply) => Some(surface(reply.preedit.as_ref().unwrap())),
        response::Body::Candidates(update) => {
            assert!(!update.pending);
            None
        }
        _ => panic!("unexpected response"),
    }
}

#[test]
fn every_clause_is_scored_and_the_accepted_spellings_are_shown_together() {
    let mut f = fixture(settings(true));
    let initial = f.convert("かいは、あめ。");
    assert_eq!(surface(initial.preedit.as_ref().unwrap()), "会は、雨。");
    f.llm.want(&["回は", "飴"]);
    assert_eq!(previewed(&mut f, &initial).as_deref(), Some("回は、飴。"));
    let script = f.llm.0.lock().unwrap();
    // One job for each clause, each with the text displayed before it as context.
    let first = script
        .jobs
        .iter()
        .find(|job| job.candidates.contains(&"回は".to_owned()))
        .unwrap();
    let second = script
        .jobs
        .iter()
        .find(|job| job.candidates.contains(&"飴".to_owned()))
        .unwrap();
    assert_eq!(first.context, CONTEXT);
    assert_eq!(second.context, format!("{CONTEXT}会は、"));
    assert_eq!(
        (first.reading.as_str(), second.reading.as_str()),
        ("カイハ", "アメ")
    );
}

#[test]
fn only_the_accepted_clause_changes_when_the_other_keeps_its_spelling() {
    let mut f = fixture(settings(true));
    let initial = f.convert("かいは、あめ。");
    f.llm.want(&["飴"]);
    assert_eq!(previewed(&mut f, &initial).as_deref(), Some("会は、飴。"));
}

#[test]
fn numbers_follow_committed_digits_after_the_words_are_kept() {
    for (wanted, shown) in [("2位だ", Some("2位だ")), ("二医だった", None)] {
        let mut f = fixture(settings(true));
        f.context.preceding_text = DIGIT_CONTEXT.into();
        let initial = f.convert("にいだ");
        assert_eq!(surface(initial.preedit.as_ref().unwrap()), "二位だ");
        f.llm.want(&[wanted]);
        assert_eq!(previewed(&mut f, &initial).as_deref(), shown);
        let script = f.llm.0.lock().unwrap();
        // The words are compared first, then the same text with its number in digits.
        let last = script.jobs.last().unwrap();
        assert_eq!(last.candidates, ["二位だ", "2位だ"]);
    }
}

#[test]
fn a_calendar_year_is_shown_with_the_words_scored_around_it() {
    for (wanted, shown) in [("2001年の飴", "2001年の飴"), ("どれでもない", "2001年の雨")]
    {
        let mut config = settings(true);
        config.calculated_dates = true;
        let mut f = fixture(config);
        let initial = f.convert("にせんいちねんのあめ");
        assert_eq!(surface(initial.preedit.as_ref().unwrap()), "二千一年の雨");
        f.llm.want(&[wanted]);
        assert_eq!(previewed(&mut f, &initial).as_deref(), Some(shown));
        let script = f.llm.0.lock().unwrap();
        // The year has one spelling; only the other segment is compared.
        let job = script.jobs.last().unwrap();
        assert_eq!(job.candidates[0], "2001年の雨");
        assert!(job.candidates.contains(&"2001年の飴".to_owned()));
        assert!(job.candidates.iter().all(|c| !c.contains("二千")));
    }
}

#[test]
fn a_calendar_year_is_shown_without_a_score_that_is_slow() {
    use std::time::{Duration, Instant};
    // Decided changes are displayed once the conversion is older than this.
    let apply = Duration::from_millis(600);
    // Nothing is displayed once it is older than this.
    let expiry = Duration::from_millis(kanade_llm::limits::RESULT_TTL_MS);
    // The second poll has to reach the engine between the two deadlines. When
    // the machine is too busy for the times measured here to show that it did,
    // the conversion is repeated. The reply never decides whether an attempt counts.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "no poll arrived between the deadlines"
        );
        let mut config = settings(true);
        config.calculated_dates = true;
        let mut f = fixture(config);
        // The engine starts timing the conversion between these two instants.
        let before = Instant::now();
        let initial = f.convert("にせんいちねんのあめ");
        let after = Instant::now();
        // No score arrives. The engine waits for it at first.
        let first = preview_poll(&mut f, &initial);
        if before.elapsed() > apply {
            continue;
        }
        assert!(matches!(first, response::Body::Candidates(update) if update.pending));
        // Past the first deadline the year needs no score.
        std::thread::sleep((apply + Duration::from_millis(1)).saturating_sub(after.elapsed()));
        let shown = previewed(&mut f, &initial);
        if before.elapsed() > expiry {
            continue;
        }
        assert_eq!(shown.as_deref(), Some("2001年の雨"));
        let waiting = f.llm.jobs() as u64;
        assert!(f.llm.cancelled().contains(&waiting));
        break;
    }
}

#[test]
fn width_only_preview_keeps_the_models_default_and_commits_the_displayed_digits() {
    let mut f = fixture(settings(true));
    f.context.preceding_text = DIGIT_CONTEXT.into();
    let initial = f.convert("さんびき");
    assert_eq!(surface(initial.preedit.as_ref().unwrap()), "３匹");
    f.llm.answer(&[Some(0.0), Some(0.0)]);
    let request = Request {
        body: Some(request::Body::Poll(CandidatePoll {
            revision: Some(f.revision),
            candidate_revision: page(&initial).candidate_revision,
            preview_event_seq: Some(f.seq + 1),
        })),
    };
    let response = f.engine.handle(f.connection, request).unwrap().unwrap();
    let Some(response::Body::Key(preview)) = response.body else {
        panic!("expected a width-only preview");
    };
    assert_eq!(surface(preview.preedit.as_ref().unwrap()), "3匹");
    assert!(preview.commit.is_none());
    f.seq = preview.event_seq;
    f.revision = preview.revision.unwrap();
    f.ack(&preview);
    let confirmed = f.special(SpecialKey::Enter);
    assert_eq!(confirmed.commit.as_ref().unwrap().value, "3匹");
}
