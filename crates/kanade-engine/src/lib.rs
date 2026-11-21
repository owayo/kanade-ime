// SPDX-License-Identifier: MIT
use kanade_ipc::*;
use kanade_learn::{Detector, Evidence, service::LearningService};
use kanade_mozc::{ConversionEngine, SessionId, Snapshot};
use std::{
    collections::HashMap,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
mod candidates;
pub mod config;
pub mod dates;
mod english;
mod refine;
mod sentence;
#[cfg(any(unix, windows))]
pub mod server;
#[cfg(windows)]
pub mod supervisor;

/// Exit codes of the engine process. Its supervisor records them, so each
/// has one meaning. A panic that ends the process leaves the runtime's 101;
/// anything else was imposed from outside.
pub mod exit {
    pub const STOPPED: u8 = 0;
    /// The reason is in the event log as a `fatal-` record.
    pub const FAILED: u8 = 1;
    /// Another engine serves this endpoint or uses this data directory.
    pub const ALREADY_RUNNING: u8 = 2;
    pub const USAGE: u8 = 64;
}

type Code = failure::Code;
impl Engine<kanade_mozc::Mozc> {
    pub fn clear_mozc_learning(&mut self, kind: kanade_mozc::LearningKind) -> Result<(), Code> {
        if self.sessions.values().any(|session| {
            session.pending.is_some() || !session.snapshot.preedit.segments.is_empty()
        }) {
            return Err(Code::Sequence);
        }
        self.backend
            .clear_learning(kind)
            .map_err(|_| Code::Unavailable)?;
        for session in self.sessions.values_mut() {
            // Do not reinsert a synthetic commit from before the clear operation.
            session.visible = None;
            session.detector = Detector::default();
        }
        Ok(())
    }
    pub fn recover_worker(&mut self) -> anyhow::Result<()> {
        for session in self.sessions.values_mut() {
            refine::cancel(self.llm.as_deref(), session);
        }
        self.sessions.clear();
        self.backend.restart()?;
        Ok(())
    }
}
struct Pending {
    undo_mozc_commit: bool,
    seq: u64,
    revision: Revision,
    commit: Option<Commit>,
    privacy: Privacy,
    preceding: String,
    automatic: bool,
    selections: Vec<Word>,
}
struct VisibleCommit {
    receipt: u64,
    value: String,
    anchor: String,
    remaining: u32,
    deleting: bool,
    history_reading: Option<String>,
}
struct Session {
    /// Protocol version negotiated for this connection.
    version: u32,
    preview_base: Option<Snapshot>,
    literal_spelling: Option<String>,
    date_candidates: Option<(String, dates::Date)>,
    id: SessionId,
    revision: Revision,
    last_seq: u64,
    last_request: Option<Request>,
    last_reply: Option<KeyReply>,
    pending: Option<Pending>,
    snapshot: Snapshot,
    candidate_revision: u64,
    detector: Detector,
    visible: Option<VisibleCommit>,
    privacy: Privacy,
    choices: Vec<candidates::Choice>,
    app: String,
    selected_choice: usize,
    preceding: String,
    automatic: bool,
    /// The user picked a candidate in this conversion; its order is final.
    manual: bool,
    refinement: Option<refine::Refinement>,
    sentence: Option<sentence::Pending>,
    applied: Option<refine::Applied>,
    frozen: Option<refine::Frozen>,
    selection_intents: Vec<Word>,
}
pub struct Engine<C> {
    spellings: &'static english::Spellings,
    emoji: &'static kanade_emoji::Catalog,
    projects: std::sync::Arc<kanade_indexer::Catalog>,
    settings: config::Config,
    backend: C,
    probe: Option<sentence::probe::Worker>,
    sessions: HashMap<u64, Session>,
    next_epoch: u64,
    next_receipt: u64,
    started: Instant,
    last_activity: Instant,
    learning: Option<LearningService>,
    date: Option<dates::Calendar>,
    llm: Option<Box<dyn kanade_llm::Rerank>>,
    /// Score leads that change a displayed conversion, registered with the model.
    preview: kanade_llm::profile::Preview,
}
impl<C: ConversionEngine> Engine<C> {
    /// Uses the thresholds of another model file. A new engine starts with
    /// those of the registered model.
    pub fn set_preview(&mut self, preview: kanade_llm::profile::Preview) {
        self.preview = preview;
    }

    /// Installs the background reranker. It is used only while `llm` is enabled.
    pub fn set_reranker(&mut self, llm: Option<Box<dyn kanade_llm::Rerank>>) {
        for session in self.sessions.values_mut() {
            refine::cancel(self.llm.as_deref(), session);
        }
        self.llm = llm;
        if self.llm.is_none()
            && let Some(probe) = &mut self.probe
        {
            probe.stop();
        }
    }
    pub fn set_date(&mut self, date: Option<dates::Date>) {
        self.date = date.map(dates::Calendar::fixed);
    }
    pub(crate) fn set_calendar(&mut self, date: Option<dates::Calendar>) {
        self.date = date;
    }
    pub fn set_projects(&mut self, projects: std::sync::Arc<kanade_indexer::Catalog>) {
        self.projects = projects;
    }
    pub fn configure(&mut self, settings: config::Config) {
        for session in self.sessions.values_mut() {
            if !settings.learning_allowed(&session.app) {
                session.selection_intents.clear();
                session.frozen = None;
                session.detector.cancel();
                session.visible = None;
                session.preceding.clear();
                refine::cancel(self.llm.as_deref(), session);
            }
        }
        if !settings.llm {
            if let Some(probe) = &mut self.probe {
                probe.stop();
            }
            for session in self.sessions.values_mut() {
                refine::cancel(self.llm.as_deref(), session);
            }
        }
        self.settings = settings;
    }
    pub fn new(backend: C, learning: Option<LearningService>) -> Self {
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        Self {
            spellings: english::Spellings::bundled(),
            emoji: kanade_emoji::Catalog::bundled(),
            projects: Default::default(),
            settings: config::Config::default(),
            probe: backend.probe_binary().map(sentence::probe::Worker::new),
            backend,
            sessions: HashMap::new(),
            next_epoch: epoch.max(1),
            next_receipt: 0,
            started: Instant::now(),
            last_activity: Instant::now(),
            learning,
            date: None,
            llm: None,
            preview: kanade_llm::profile::CATALOG[0].preview,
        }
    }
    pub fn open(&mut self, hello: &Hello) -> Result<Welcome, Code> {
        let version = negotiate(hello).ok_or(Code::Invalid)?;
        if self.sessions.len() >= 32 {
            return Err(Code::Unavailable);
        }
        let connection = self.next_epoch.checked_add(1).ok_or(Code::Unavailable)?;
        self.next_epoch = connection;
        self.backend
            .set_privacy(Privacy::Unknown)
            .map_err(|_| Code::Unavailable)?;
        let id = self
            .backend
            .create_session()
            .map_err(|_| Code::Unavailable)?;
        self.sessions.insert(
            connection,
            Session {
                version,
                preview_base: None,
                literal_spelling: None,
                date_candidates: None,
                id,
                revision: Revision {
                    connection,
                    context: 1,
                    composition: 0,
                },
                last_seq: 0,
                last_request: None,
                last_reply: None,
                pending: None,
                snapshot: Snapshot::default(),
                candidate_revision: 0,
                detector: Detector::default(),
                visible: None,
                privacy: Privacy::Unknown,
                choices: vec![],
                app: String::new(),
                selected_choice: 0,
                preceding: String::new(),
                automatic: false,
                manual: false,
                refinement: None,
                sentence: None,
                applied: None,
                frozen: None,
                selection_intents: vec![],
            },
        );
        Ok(Welcome {
            version,
            connection,
            learning_apps: vec![],
            learning_policy: Some(LearningPolicy {
                enabled: self.settings.learning,
                excluded_apps: self.settings.learning_excluded_apps.clone(),
            }),
        })
    }
    pub fn close(&mut self, connection: u64) {
        if let Some(mut session) = self.sessions.remove(&connection) {
            refine::cancel(self.llm.as_deref(), &mut session);
            // The frontend left without acknowledging this commit, so the
            // document may never have received it. Do not let the worker keep
            // what it learned from it.
            if session
                .pending
                .as_ref()
                .is_some_and(|pending| pending.undo_mozc_commit)
            {
                let _ = self.backend.undo_learning(session.id);
            }
            let _ = self.backend.delete_session(session.id);
        }
    }
    /// False once the worker process must be replaced. Other failures end one
    /// session and leave the worker, and every other composition, running.
    pub fn worker_healthy(&self) -> bool {
        self.backend.healthy()
    }
    /// A composition is displayed or a document edit awaits acknowledgement.
    pub fn composing(&self) -> bool {
        self.sessions.values().any(|session| {
            session.pending.is_some() || !session.snapshot.preedit.segments.is_empty()
        })
    }
    pub fn sync(&mut self) -> Result<(), Code> {
        self.backend.sync().map_err(|_| Code::Unavailable)
    }
    pub fn flush_learning(&self) -> std::io::Result<()> {
        if let Some(learning) = &self.learning {
            learning.flush()?;
        }
        Ok(())
    }
    pub fn handle(&mut self, connection: u64, request: Request) -> Result<Option<Response>, Code> {
        let mut session = self.sessions.remove(&connection).ok_or(Code::Stale)?;
        self.backend
            .set_deadline(Some(Instant::now() + WORKER_KEY_TIMEOUT));
        let result = self.process(&mut session, &request);
        self.backend.set_deadline(None);
        // The conversion state of this session is uncertain. End this
        // connection; a healthy worker keeps serving the others.
        if result == Err(Code::Unavailable) {
            refine::cancel(self.llm.as_deref(), &mut session);
            if self.backend.healthy() {
                let _ = self.backend.delete_session(session.id);
            }
        } else {
            self.sessions.insert(connection, session);
        }
        result
    }
    fn process(
        &mut self,
        session: &mut Session,
        request: &Request,
    ) -> Result<Option<Response>, Code> {
        match request.body.as_ref().ok_or(Code::Invalid)? {
            request::Body::Key(key) => {
                validate_key(key)?;
                if let Some(reply) = duplicate(session, request, key.event_seq)? {
                    return Ok(Some(key_response(reply)));
                }
                check_revision(session, key.revision.as_ref(), key.event_seq)?;
                if session.pending.is_some() {
                    return Err(Code::Sequence);
                }
                // Any new event ends a background reordering of the old list.
                refine::cancel(self.llm.as_deref(), session);
                let context = key.context.as_ref().ok_or(Code::Invalid)?;
                let mut privacy = Privacy::try_from(context.privacy).map_err(|_| Code::Privacy)?;
                if privacy == Privacy::Normal && !self.settings.learning_allowed(&context.app) {
                    privacy = Privacy::Unknown;
                }
                if privacy == Privacy::Password {
                    session.literal_spelling = None;
                    session.detector.cancel();
                    session.visible = None;
                    session.frozen = None;
                    return Err(Code::Privacy);
                }
                self.last_activity = Instant::now();
                // A global setting is switched before each session can send input.
                self.backend
                    .set_privacy(privacy)
                    .map_err(|_| Code::Unavailable)?;
                if privacy != Privacy::Normal
                    || session.privacy != privacy
                    || session.app != context.app
                {
                    session.selection_intents.clear();
                    session.frozen = None;
                }
                if session.privacy != privacy || session.app != context.app {
                    session.literal_spelling = None;
                }
                session.privacy = privacy;
                session.app.clone_from(&context.app);
                let preceding = if privacy == Privacy::Normal {
                    context.preceding_text.as_str()
                } else {
                    ""
                };
                session.preceding = preceding.to_owned();
                let input = key.key.as_ref().ok_or(Code::Invalid)?;
                if privacy == Privacy::Normal
                    && context.surrounding_available
                    && !input.text.is_empty()
                    && session.snapshot.preedit.segments.is_empty()
                    && let Some(visible) = session.visible.as_mut()
                    && !visible.deleting
                    && preceding.strip_prefix(&visible.anchor) == Some(visible.value.as_str())
                    && let Some(reading) = visible.history_reading.take()
                {
                    self.backend
                        .add_history(&reading, &visible.value)
                        .map_err(|_| Code::Unavailable)?;
                }
                if context.surrounding_available {
                    self.observe_deletion(session, input, preceding, privacy);
                } else {
                    self.observe_weak_deletion(session, input, privacy);
                }
                let was_converting = session.snapshot.converting;
                let was_composing = !session.snapshot.preedit.segments.is_empty();
                let history_delete = kanade_ipc::frontend::is_history_delete(input);
                let overlay = if history_delete {
                    Some(self.delete_selected_history(session, privacy, key.passed_through)?)
                } else if !key.passed_through {
                    self.candidate_key(session, input, preceding)?
                } else {
                    None
                };
                // Candidate navigation, digits and Enter are manual choices.
                session.manual |= overlay.is_some() && !history_delete;
                let mut snapshot = match overlay {
                    Some(snapshot) => snapshot,
                    None => {
                        let snapshot = self
                            .backend
                            .key(session.id, input, preceding)
                            .map_err(|_| Code::Unavailable)?;
                        // Segment movement/resizing and cancellation operate on the
                        // backend composition, ending an earlier synthetic preview.
                        session.preview_base = None;
                        snapshot
                    }
                };
                if key.passed_through && (snapshot.consumed || snapshot.commit.is_some()) {
                    if was_composing || snapshot.commit.is_some() {
                        return Err(Code::Unavailable);
                    }
                    // The frontend left this key to the application, but the
                    // worker started a composition with it. Nothing was shown,
                    // so discard that composition and report the key unused.
                    self.backend
                        .reset(session.id)
                        .map_err(|_| Code::Unavailable)?;
                    snapshot = Snapshot::default();
                }
                if self.settings.punctuation_commit
                    && !history_delete
                    && (snapshot.commit.is_none()
                        || snapshot.commit.as_ref().is_some_and(|c| c.synthetic))
                    && !snapshot.converting
                {
                    let value = snapshot
                        .preedit
                        .segments
                        .iter()
                        .map(|s| s.value.as_str())
                        .collect::<String>();
                    let mut chars = value.chars().rev();
                    if chars
                        .next()
                        .is_some_and(|c| matches!(c, '。' | '、' | '？' | '！' | '?' | '!'))
                        && !chars.next().is_some_and(char::is_numeric)
                        && snapshot.preedit.cursor_utf16 == utf16_len(&value)
                    {
                        let prefix = snapshot.commit.take();
                        snapshot = self
                            .backend
                            .submit(session.id)
                            .map_err(|_| Code::Unavailable)?;
                        if let Some(prefix) = prefix {
                            snapshot.commit =
                                Some(candidates::append_commit(prefix, snapshot.commit.take()));
                        }
                    }
                }
                let mut automatic = false;
                if !was_converting && snapshot.converting {
                    session.automatic = false;
                }
                if was_converting
                    && (input.special != SpecialKey::Enter as i32 || !input.text.is_empty())
                {
                    session.automatic = false;
                }
                let prefer_initial = self.settings.learning
                    && privacy == Privacy::Normal
                    && !was_converting
                    && snapshot.converting
                    && !key.passed_through
                    && !history_delete
                    && !kanade_ipc::frontend::is_conversion_control(input)
                    && snapshot.commit.is_none();
                english::update_literal(
                    &mut session.literal_spelling,
                    &session.snapshot,
                    &snapshot,
                    input,
                );
                if !snapshot.synthetic_preview {
                    let preferred = self.refresh_choices(session, &snapshot, &context.app);
                    if prefer_initial
                        && let Some(value) = preferred
                        && snapshot
                            .preedit
                            .segments
                            .get(snapshot.focused_segment)
                            .is_some_and(|s| s.value != value)
                    {
                        // Reuse the same published preference snapshot for both
                        // the selected surface and the displayed order.
                        snapshot = self
                            .backend
                            .select(session.id, &value)
                            .map_err(|_| Code::Unavailable)?;
                        automatic = true;
                        session.automatic = true;
                        let _ = self.refresh_choices(session, &snapshot, &context.app);
                    }
                }
                if !snapshot.converting {
                    session.manual = false;
                }
                automatic |= session.automatic;
                let refinable = context.surrounding_available
                    && !key.passed_through
                    && !snapshot.synthetic_preview
                    && snapshot.commit.is_none();
                let mut reply = self.reply(
                    session,
                    key.event_seq,
                    snapshot,
                    preceding.to_owned(),
                    privacy,
                    automatic,
                )?;
                if refinable {
                    self.refine(session, &mut reply, privacy, preceding);
                    if !was_converting
                        && self.settings.llm
                        && self.llm.is_some()
                        && privacy == Privacy::Normal
                        && let Some(pending) = sentence::Pending::prepare(
                            session,
                            preceding,
                            self.settings.calculated_dates,
                        )
                        && let Some(page) = &mut reply.candidates
                        && page.refinement_outcome != RefinementOutcome::Unavailable as i32
                    {
                        session.sentence = Some(pending);
                        page.refinement_pending = true;
                    }
                }
                session.last_request = Some(request.clone());
                session.last_reply = Some(reply.clone());
                Ok(Some(key_response(reply)))
            }
            request::Body::Select(select) => {
                if let Some(reply) = duplicate(session, request, select.event_seq)? {
                    return Ok(Some(key_response(reply)));
                }
                check_revision(session, select.revision.as_ref(), select.event_seq)?;
                if session.pending.is_some() {
                    return Err(Code::Sequence);
                }
                refine::cancel(self.llm.as_deref(), session);
                let handle = select.handle.ok_or(Code::Invalid)?;
                if handle.session_epoch != session.revision.connection
                    || handle.candidate_revision != session.candidate_revision
                {
                    return Err(Code::Stale);
                }
                let choice = session
                    .choices
                    .get(handle.slot as usize)
                    .ok_or(Code::Stale)?
                    .clone();
                self.last_activity = Instant::now();
                if session.privacy == Privacy::Normal
                    && !self.settings.learning_allowed(&session.app)
                {
                    session.privacy = Privacy::Unknown;
                    session.selection_intents.clear();
                    session.frozen = None;
                }
                self.backend
                    .set_privacy(session.privacy)
                    .map_err(|_| Code::Unavailable)?;
                let snapshot = self.choose(session, &choice, select.submit)?;
                candidates::remember_selection(session, &choice);
                session.automatic = false;
                session.manual = snapshot.converting;
                session.selected_choice = handle.slot as usize;
                let reply = self.reply(
                    session,
                    select.event_seq,
                    snapshot,
                    session.preceding.clone(),
                    session.privacy,
                    false,
                )?;
                session.last_request = Some(request.clone());
                session.last_reply = Some(reply.clone());
                Ok(Some(key_response(reply)))
            }
            request::Body::Ack(ack) => {
                if ack.event_seq == session.last_seq && session.pending.is_none() {
                    return if ack.revision == Some(session.revision) {
                        Ok(None)
                    } else {
                        Err(Code::Stale)
                    };
                }
                let pending = session.pending.as_ref().ok_or(Code::Sequence)?;
                if pending.seq != ack.event_seq || ack.revision != Some(pending.revision) {
                    return Err(Code::Stale);
                }
                let pending = session.pending.take().ok_or(Code::Sequence)?;
                if !ack.applied {
                    session.detector.cancel();
                    session.visible = None;
                    if pending.undo_mozc_commit {
                        let _ = self.backend.undo_learning(session.id);
                    }
                    self.backend
                        .reset(session.id)
                        .map_err(|_| Code::Unavailable)?;
                    return Err(Code::Unavailable);
                }
                if let Some(commit) = pending.commit {
                    self.next_receipt =
                        self.next_receipt.checked_add(1).ok_or(Code::Unavailable)?;
                    let receipt = self.next_receipt;
                    let now = self.started.elapsed().as_millis() as u64;
                    let allowed = self.settings.learning
                        && pending.privacy == Privacy::Normal
                        && self.settings.learning_allowed(&session.app);
                    let corrections = session.detector.confirmed(
                        receipt,
                        session.revision.context,
                        commit.clone(),
                        if allowed {
                            Privacy::Normal
                        } else {
                            Privacy::Unknown
                        },
                        now,
                    );
                    let corrections = if pending.automatic {
                        vec![]
                    } else {
                        corrections
                    };
                    if allowed
                        && (!corrections.is_empty() || !pending.selections.is_empty())
                        && let Some(learning) = &self.learning
                    {
                        learning.learned(receipt, corrections, pending.selections, now);
                    }
                    session.visible = if allowed && !commit.transient {
                        Some(VisibleCommit {
                            receipt,
                            value: commit.value.clone(),
                            anchor: pending.preceding,
                            remaining: utf16_len(&commit.value),
                            deleting: false,
                            history_reading: commit.synthetic.then_some(commit.reading.clone()),
                        })
                    } else {
                        None
                    };
                }
                Ok(None)
            }
            request::Body::Reset(reset) => {
                if reset.context <= session.revision.context {
                    return Err(Code::Stale);
                }
                refine::cancel(self.llm.as_deref(), session);
                session.manual = false;
                session.frozen = None;
                session.selection_intents.clear();
                self.backend
                    .reset(session.id)
                    .map_err(|_| Code::Unavailable)?;
                session.revision.context = reset.context;
                session.revision.composition = 0;
                session.last_seq = 0;
                session.last_request = None;
                session.last_reply = None;
                session.pending = None;
                session.detector.cancel();
                session.visible = None;
                session.snapshot = Snapshot::default();
                session.preview_base = None;
                session.preceding.clear();
                session.choices.clear();
                session.date_candidates = None;
                session.literal_spelling = None;
                session.automatic = false;
                Ok(Some(Response {
                    body: Some(response::Body::Ack(true)),
                }))
            }
            request::Body::Restore(restore) => {
                if session.version < RESTORE_VERSION {
                    return Err(Code::Invalid);
                }
                if let Some(reply) = duplicate(session, request, restore.event_seq)? {
                    return Ok(Some(key_response(reply)));
                }
                check_revision(session, restore.revision.as_ref(), restore.event_seq)?;
                // Only the first event of a connection may adopt a reading:
                // the session has no composition, edit or history to disturb.
                if session.last_seq != 0
                    || session.pending.is_some()
                    || !session.snapshot.preedit.segments.is_empty()
                {
                    return Err(Code::Sequence);
                }
                let context = restore.context.as_ref().ok_or(Code::Invalid)?;
                if restore.reading.is_empty()
                    || restore.reading.len() > MAX_PREEDIT
                    || restore.reading.chars().any(char::is_control)
                    || context.preceding_text.len() > MAX_CONTEXT
                    || context.app.len() > 256
                {
                    return Err(Code::Invalid);
                }
                let mut privacy = Privacy::try_from(context.privacy).map_err(|_| Code::Privacy)?;
                if privacy == Privacy::Normal && !self.settings.learning_allowed(&context.app) {
                    privacy = Privacy::Unknown;
                }
                if privacy == Privacy::Password {
                    return Err(Code::Privacy);
                }
                self.last_activity = Instant::now();
                // As for a key: learning is switched off before any text of a
                // private or unknown context reaches the worker.
                self.backend
                    .set_privacy(privacy)
                    .map_err(|_| Code::Unavailable)?;
                session.privacy = privacy;
                session.app.clone_from(&context.app);
                let preceding = if privacy == Privacy::Normal {
                    context.preceding_text.as_str()
                } else {
                    ""
                };
                session.preceding = preceding.to_owned();
                session.literal_spelling = None;
                let snapshot = match self.backend.restore(session.id, &restore.reading) {
                    Ok(snapshot) => snapshot,
                    // The session is empty again; only this request failed.
                    Err(kanade_mozc::Error::Unrestorable) => return Err(Code::Invalid),
                    Err(_) => return Err(Code::Unavailable),
                };
                if snapshot.commit.is_some()
                    || snapshot.converting
                    || reading(&snapshot.preedit) != restore.reading
                {
                    self.backend
                        .reset(session.id)
                        .map_err(|_| Code::Unavailable)?;
                    return Err(Code::Invalid);
                }
                let _ = self.refresh_choices(session, &snapshot, &context.app);
                let reply = self.reply(
                    session,
                    restore.event_seq,
                    snapshot,
                    preceding.to_owned(),
                    privacy,
                    false,
                )?;
                session.last_request = Some(request.clone());
                session.last_reply = Some(reply.clone());
                Ok(Some(key_response(reply)))
            }
            request::Body::Poll(poll) => {
                if let Some(seq) = poll.preview_event_seq
                    && session.last_request.as_ref() == Some(request)
                    && let Some(reply) = duplicate(session, request, seq)?
                {
                    return Ok(Some(key_response(reply)));
                }
                if session.sentence.is_none() || poll.preview_event_seq.is_none() {
                    return self.poll(session, poll);
                }
                let result = self.poll_sentence(session, poll)?;
                // Modern clients also get the normal candidate ordering when the
                // whole-reading comparison has no confident correction.
                if session.refinement.is_some()
                    && session.sentence.is_none()
                    && matches!(
                        result.as_ref().and_then(|r| r.body.as_ref()),
                        Some(response::Body::Candidates(_))
                    )
                {
                    return self.poll(session, poll);
                }
                if let Some(response::Body::Candidates(update)) =
                    result.as_ref().and_then(|r| r.body.as_ref())
                    && !update.pending
                    && update.revision == poll.revision
                    && poll.revision == Some(session.revision)
                    && let Some(page) = session
                        .last_reply
                        .as_mut()
                        .and_then(|r| r.candidates.as_mut())
                    && page.candidate_revision == poll.candidate_revision
                {
                    page.refinement_pending = false;
                    page.refinement_outcome = update.refinement_outcome;
                }
                Ok(result)
            }
            _ => Err(Code::Invalid),
        }
    }
    fn observe_deletion(
        &self,
        session: &mut Session,
        key: &Key,
        preceding: &str,
        privacy: Privacy,
    ) {
        if privacy != Privacy::Normal {
            session.detector.cancel();
            session.visible = None;
            return;
        }
        let Some(visible) = session.visible.as_mut() else {
            return;
        };
        if visible.deleting {
            let remainder = preceding
                .strip_prefix(&visible.anchor)
                .filter(|tail| visible.value.starts_with(tail));
            if let Some(remainder) = remainder {
                let remaining = utf16_len(remainder);
                if remaining < visible.remaining {
                    session.detector.deletion(
                        session.revision.context,
                        remaining,
                        Evidence::Strong,
                        self.started.elapsed().as_millis() as u64,
                    );
                    visible.remaining = remaining;
                    if let Some(learning) = &self.learning {
                        learning.undo(visible.receipt);
                    }
                }
            } else {
                session.detector.cancel();
                session.visible = None;
                return;
            }
        }
        if key.special == SpecialKey::Backspace as i32 {
            visible.deleting = true;
        } else if (key.text.is_empty() && session.snapshot.preedit.segments.is_empty())
            || key.modifiers & (MOD_CONTROL | MOD_ALT | MOD_META) != 0
        {
            session.detector.cancel();
            session.visible = None;
        }
    }
    fn observe_weak_deletion(&self, session: &mut Session, key: &Key, privacy: Privacy) {
        let composing = !session.snapshot.preedit.segments.is_empty();
        if privacy != Privacy::Normal
            || key.modifiers != 0
            || (!composing && key.text.is_empty() && key.special != SpecialKey::Backspace as i32)
        {
            session.detector.cancel();
            session.visible = None;
            return;
        }
        let Some(visible) = session.visible.as_mut() else {
            return;
        };
        if key.special == SpecialKey::Backspace as i32 && !composing {
            if visible.remaining == 0
                || !visible
                    .value
                    .chars()
                    .all(|c| matches!(c,'ぁ'..='ゖ'|'ァ'..='ヶ'|'一'..='龯'))
            {
                session.detector.cancel();
                session.visible = None;
                return;
            }
            visible.remaining -= 1;
            visible.deleting = true;
            let receipt = session.detector.deletion(
                session.revision.context,
                visible.remaining,
                Evidence::Weak,
                self.started.elapsed().as_millis() as u64,
            );
            if let Some(receipt) = receipt
                && let Some(learning) = &self.learning
            {
                learning.undo(receipt);
            }
        }
    }
    fn reply(
        &self,
        session: &mut Session,
        seq: u64,
        snapshot: Snapshot,
        preceding: String,
        privacy: Privacy,
        automatic: bool,
    ) -> Result<KeyReply, Code> {
        // Only explicitly chosen terms present in the actual commit are eligible.
        // Navigating, cancelling or editing a composition is not evidence.
        let selections = snapshot
            .commit
            .as_ref()
            .filter(|c| !c.transient)
            .map(|commit| {
                session
                    .selection_intents
                    .iter()
                    .filter(|word| commit.words.contains(word))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        session.selection_intents.retain(|word| {
            snapshot
                .preedit
                .segments
                .iter()
                .any(|s| s.reading == word.reading && s.value == word.value)
        });
        session.revision.composition = session
            .revision
            .composition
            .checked_add(1)
            .ok_or(Code::Unavailable)?;
        session.candidate_revision = session
            .candidate_revision
            .checked_add(1)
            .ok_or(Code::Unavailable)?;
        session.last_seq = seq;
        let composing = !snapshot.preedit.segments.is_empty();
        if !composing {
            session.literal_spelling = None;
        }
        let page = self.page(session, &snapshot);
        let reply = KeyReply {
            revision: Some(session.revision),
            event_seq: seq,
            state_revision: session.revision.composition,
            consumed: snapshot.consumed,
            preedit: Some(snapshot.preedit.clone()),
            commit: snapshot.commit.clone(),
            candidates: page,
            key_policy: Some(KeyPolicySummary {
                composing,
                enabled: true,
                can_undo_learning: snapshot.commit.is_some(),
            }),
        };
        session.pending = Some(Pending {
            undo_mozc_commit: snapshot.undo_mozc_commit,
            seq,
            revision: session.revision,
            commit: snapshot.commit.clone(),
            privacy,
            preceding,
            automatic,
            selections,
        });
        // Commits belong to this reply/pending ACK, never to the steady state used
        // by later synthetic previews or no-op commands. The retry cache keeps it.
        if !snapshot.synthetic_preview {
            session.preview_base = None;
        }
        session.snapshot = Snapshot {
            commit: None,
            undo_mozc_commit: false,
            ..snapshot
        };
        Ok(reply)
    }
}
impl<C> Engine<C> {
    /// The displayed page for the session's current choices and selection.
    fn page(&self, session: &Session, snapshot: &Snapshot) -> Option<CandidatePage> {
        if !snapshot.converting || session.choices.is_empty() {
            return None;
        }
        let page_size = self.settings.page_size.clamp(1, 9) as usize;
        let start = (session.selected_choice / page_size) * page_size;
        Some(CandidatePage {
            candidate_revision: session.candidate_revision,
            total: session.choices.len() as u32,
            start: start as u32,
            selected: session.selected_choice as u32,
            items: session
                .choices
                .iter()
                .enumerate()
                .skip(start)
                .take(page_size)
                .map(|(slot, c)| CandidateItem {
                    handle: Some(CandidateHandle {
                        session_epoch: session.revision.connection,
                        candidate_revision: session.candidate_revision,
                        slot: slot as u32,
                    }),
                    value: c.value.clone(),
                    reading: c.reading.clone(),
                    annotation: c.annotation.clone(),
                    source: c.source as i32,
                    number: (slot - start + 1) as u32,
                })
                .collect(),
            footer: reading(&snapshot.preedit),
            font_size: self.settings.font_size,
            refinement_pending: false,
            refinement_outcome: if self.settings.llm
                && session.privacy == Privacy::Normal
                && session.frozen.as_ref().is_some_and(|order| order.evaluated)
            {
                RefinementOutcome::Evaluated as i32
            } else {
                RefinementOutcome::Unspecified as i32
            },
        })
    }
}
fn check_revision(session: &Session, revision: Option<&Revision>, seq: u64) -> Result<(), Code> {
    if revision != Some(&session.revision) {
        return Err(Code::Stale);
    }
    if seq != session.last_seq.checked_add(1).ok_or(Code::Sequence)? {
        return Err(Code::Sequence);
    }
    Ok(())
}
fn duplicate(session: &Session, request: &Request, seq: u64) -> Result<Option<KeyReply>, Code> {
    if seq != session.last_seq {
        return Ok(None);
    }
    if session.last_request.as_ref() != Some(request) {
        return Err(Code::Sequence);
    }
    Ok(session.last_reply.clone())
}
fn key_response(reply: KeyReply) -> Response {
    Response {
        body: Some(response::Body::Key(reply)),
    }
}
fn validate_key(request: &KeyRequest) -> Result<(), Code> {
    let key = request.key.as_ref().ok_or(Code::Invalid)?;
    let context = request.context.as_ref().ok_or(Code::Invalid)?;
    if key.text.len() > 32
        || key.text.contains('\0')
        || context.preceding_text.len() > MAX_CONTEXT
        || context.app.len() > 256
        || key.modifiers > 15
        || SpecialKey::try_from(key.special).is_err()
        || (key.special != SpecialKey::None as i32 && !key.text.is_empty())
        || (key.special == SpecialKey::None as i32 && key.text.is_empty())
    {
        return Err(Code::Invalid);
    }
    Ok(())
}
