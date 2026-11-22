use crate::{
    diagnostics::{Event, record},
    *,
};
use std::io;

/// Native code applies edits and reports whether the document accepted them.
pub trait Document {
    fn apply(&mut self, reply: &KeyReply) -> bool;
    fn finish_reading(&mut self, reading: &str) -> bool;
    /// Removes the displayed composition without inserting text.
    fn cancel(&mut self) -> bool {
        self.apply(&KeyReply {
            preedit: Some(Preedit::default()),
            ..Default::default()
        })
    }
}
pub trait Transport {
    fn exchange(&mut self, request: &Request) -> io::Result<Response>;
    fn notify(&mut self, request: &Request) -> io::Result<()>;
    /// False once the peer is known to have closed. Sends nothing.
    fn is_open(&mut self) -> bool {
        true
    }
}

/// Canonical commands for the pinned MS-IME keymap, shared by both frontends.
pub fn function_key(number: u32, modifiers: u32) -> Option<Key> {
    if modifiers != 0 {
        return None;
    }
    let text = match number {
        6 => "u",
        7 => "i",
        8 => "o",
        9 => "p",
        10 => "t",
        _ => return None,
    };
    Some(Key {
        text: text.into(),
        modifiers: MOD_CONTROL,
        ..Default::default()
    })
}

pub fn is_conversion_control(key: &Key) -> bool {
    key.modifiers == MOD_CONTROL
        && key.special == SpecialKey::None as i32
        && matches!(key.text.as_str(), "u" | "i" | "o" | "p" | "t")
}
/// A printable key that an idle conversion session can own.
pub fn starts_composition(key: &Key) -> bool {
    key.modifiers & (MOD_CONTROL | MOD_ALT | MOD_META) == 0
        && !key.text.is_empty()
        && key.text.bytes().all(|c| c.is_ascii_graphic())
}

/// Matches the pinned keymap's explicit history-removal command, not text deletion.
pub fn is_history_delete(key: &Key) -> bool {
    key.text.is_empty() && key.special == SpecialKey::Delete as i32 && key.modifiers == MOD_CONTROL
}
#[cfg(unix)]
impl Transport for crate::unix::DeadlineStream {
    fn exchange(&mut self, request: &Request) -> io::Result<Response> {
        self.begin(KEY_TIMEOUT);
        send(self, request)?;
        receive(self)
    }
    fn notify(&mut self, request: &Request) -> io::Result<()> {
        self.begin(KEY_TIMEOUT);
        send(self, request)
    }
    fn is_open(&mut self) -> bool {
        self.peer_open()
    }
}
/// Restore attempts that may fail in transport before the hold stops trying.
const RESTORE_ATTEMPTS: u8 = 3;

/// One frontend composition and its engine session.
///
/// When the session is lost while a composition is displayed, the client
/// *holds* it: the document keeps its marked text and the client keeps the
/// last reading the document acknowledged. A hold never ends by itself. No
/// timeout, retry limit or age inserts text. It ends when
/// - a new session accepts the reading again (`reattach`, then the next key),
/// - the user presses Enter, which inserts the reading,
/// - the user presses Escape, which removes the composition, or
/// - the native side reports the end of the focus, source or document
///   (`finish`), which inserts the reading.
///
/// The key whose exchange failed is never sent again.
pub struct Client<T> {
    learning_apps: Vec<String>,
    learning_policy: Option<LearningPolicy>,
    transport: Option<T>,
    version: u32,
    revision: Revision,
    event_seq: u64,
    applied_seq: u64,
    last_reading: String,
    policy: KeyPolicySummary,
    blocked: bool,
    held: bool,
    /// The hold cannot be restored; only an explicit action ends it.
    stranded: bool,
    restore_failures: u8,
}
enum Held {
    /// A new session has the reading; process the key normally.
    Restored,
    /// The key ended or kept the hold and must not reach the application.
    Used,
    /// A chord the composition does not own; the application handles it.
    Passed,
}
fn handshake<T: Transport>(transport: &mut T) -> io::Result<Welcome> {
    let response = transport.exchange(&Request {
        body: Some(request::Body::Hello(Hello {
            min_version: OLDEST_VERSION,
            max_version: CURRENT_VERSION,
        })),
    })?;
    let Some(response::Body::Welcome(welcome)) = response.body else {
        return Err(io::ErrorKind::InvalidData.into());
    };
    if !(OLDEST_VERSION..=CURRENT_VERSION).contains(&welcome.version) || welcome.connection == 0 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(welcome)
}
impl<T: Transport> Client<T> {
    pub fn connect(mut transport: T) -> io::Result<Self> {
        let welcome = handshake(&mut transport)?;
        Ok(Self {
            learning_apps: welcome.learning_apps,
            learning_policy: welcome.learning_policy,
            transport: Some(transport),
            version: welcome.version,
            revision: Revision {
                connection: welcome.connection,
                context: 1,
                composition: 0,
            },
            event_seq: 0,
            applied_seq: 0,
            last_reading: String::new(),
            policy: KeyPolicySummary {
                enabled: true,
                ..Default::default()
            },
            blocked: false,
            held: false,
            stranded: false,
            restore_failures: 0,
        })
    }
    /// Whether the native side should open a connection and call `reattach`.
    /// Also notices a connection the engine closed while nothing was being
    /// sent, so the next key goes to a new session instead of failing.
    pub fn needs_transport(&mut self) -> bool {
        if self.transport.as_mut().is_some_and(|t| !t.is_open()) {
            record(Event::ConnectionClosedWhileIdle);
            self.detach();
        }
        !self.blocked && self.transport.is_none() && !(self.held && self.stranded)
    }
    /// Adopts a new connection in place of a lost one. Sequence numbers start
    /// again; a held composition stays held until the next key restores it.
    pub fn reattach(&mut self, mut transport: T) -> io::Result<()> {
        if self.blocked || self.transport.is_some() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let welcome = handshake(&mut transport)?;
        self.learning_apps = welcome.learning_apps;
        self.learning_policy = welcome.learning_policy;
        self.transport = Some(transport);
        self.version = welcome.version;
        self.revision = Revision {
            connection: welcome.connection,
            context: 1,
            composition: 0,
        };
        self.event_seq = 0;
        self.applied_seq = 0;
        record(Event::Reconnected);
        Ok(())
    }
    /// TSF OnTestKeyDown must only consult this local summary.
    pub fn would_consume(&self, key: &Key, privacy: Privacy) -> bool {
        if privacy == Privacy::Password
            || self.blocked
            || (self.transport.is_none() && !self.held)
            || !self.policy.enabled
        {
            return false;
        }
        if key.modifiers & (MOD_CONTROL | MOD_ALT | MOD_META) != 0 {
            return self.policy.composing
                && (key.special != SpecialKey::None as i32 || is_conversion_control(key));
        }
        self.policy.composing || starts_composition(key)
    }
    pub fn privacy_for_app(&self, app: &str) -> Privacy {
        let allowed = if let Some(policy) = &self.learning_policy {
            policy.enabled && learning_app_allowed(app, &policy.excluded_apps)
        } else {
            // Older engines still publish an allowlist. An absent new policy
            // must never make their scope broader.
            learning_app_allowed(app, &[])
                && self.learning_apps.iter().any(|allowed| allowed == app)
        };
        if allowed {
            Privacy::Normal
        } else {
            Privacy::Unknown
        }
    }
    pub fn key(
        &mut self,
        key: Key,
        context: Context,
        passed_through: bool,
        document: &mut impl Document,
    ) -> bool {
        self.key_result(key, context, passed_through, document)
            .unwrap_or(self.held || self.blocked)
    }
    /// None when the key did not get a valid reply. An applied reply still
    /// has its own result if the subsequent acknowledgement could not be sent.
    pub fn key_result(
        &mut self,
        key: Key,
        context: Context,
        passed_through: bool,
        document: &mut impl Document,
    ) -> Option<bool> {
        if context.privacy == Privacy::Password as i32 {
            self.transport = None;
            self.last_reading.clear();
            self.policy = KeyPolicySummary::default();
            self.blocked = false;
            self.held = false;
            self.stranded = false;
            return Some(false);
        }
        if self.blocked {
            return Some(true);
        }
        if self.held {
            match self.resume(&key, &context, passed_through, document) {
                Held::Restored => {}
                Held::Used => return Some(true),
                Held::Passed => return Some(false),
            }
        }
        let Some(seq) = self.event_seq.checked_add(1) else {
            self.recover();
            return None;
        };
        self.event_seq = seq;
        let request = Request {
            body: Some(request::Body::Key(KeyRequest {
                revision: Some(self.revision),
                event_seq: seq,
                key: Some(key),
                context: Some(context),
                passed_through,
            })),
        };
        self.request(request, seq, document)
    }
    pub fn is_connected(&self) -> bool {
        self.transport.is_some()
    }
    /// The document rejected an edit. A frontend that has ended the native
    /// composition can drop this client and start the next key in a new session.
    pub fn edit_rejected(&self) -> bool {
        self.blocked
    }
    /// A composition is displayed without an engine session.
    pub fn is_held(&self) -> bool {
        self.held
    }
    /// Refresh candidate ordering only. This never applies a document edit,
    /// advances the key sequence, or acknowledges a commit.
    pub fn poll_candidates(
        &mut self,
        current: &CandidatePage,
    ) -> io::Result<Option<CandidatePage>> {
        self.poll(current, None)
    }
    /// A focused native editor may accept a revision-bound provisional spelling.
    pub fn poll_with_preview(
        &mut self,
        current: &CandidatePage,
        document: &mut impl Document,
    ) -> io::Result<Option<CandidatePage>> {
        self.poll(current, Some(document))
    }
    fn poll(
        &mut self,
        current: &CandidatePage,
        document: Option<&mut dyn Document>,
    ) -> io::Result<Option<CandidatePage>> {
        if self.blocked
            || !self.policy.composing
            || !current.refinement_pending
            || self.transport.is_none()
        {
            return Ok(None);
        }
        let preview_event_seq = document
            .as_ref()
            .and_then(|_| self.event_seq.checked_add(1));
        let request = Request {
            body: Some(request::Body::Poll(CandidatePoll {
                preview_event_seq,
                revision: Some(self.revision),
                candidate_revision: current.candidate_revision,
            })),
        };
        let result = self.transport.as_mut().unwrap().exchange(&request);
        let result = match result {
            Ok(Response {
                body: Some(response::Body::Key(reply)),
            }) if preview_event_seq == Some(reply.event_seq)
                && self.matches(&reply, reply.event_seq)
                && reply.commit.is_none()
                && reply
                    .preedit
                    .as_ref()
                    .is_some_and(|p| reading(p) == self.last_reading)
                && reply.revision.is_some_and(|r| {
                    Some(r.composition) == self.revision.composition.checked_add(1)
                })
                && reply.candidates.as_ref().is_some_and(|p| {
                    !p.refinement_pending && p.candidate_revision > current.candidate_revision
                }) =>
            {
                self.event_seq = reply.event_seq;
                if document.is_some_and(|doc| self.accept(&reply, reply.event_seq, doc)) {
                    Ok(reply.candidates)
                } else {
                    Err(io::ErrorKind::InvalidData.into())
                }
            }
            Ok(Response {
                body: Some(response::Body::Candidates(update)),
            }) => {
                if update.revision != Some(self.revision) {
                    return Ok(None);
                }
                match update.candidates {
                    Some(page)
                        if !update.pending
                            && valid_refinement(&page, current, self.revision.connection) =>
                    {
                        Ok(Some(page))
                    }
                    Some(_) => Err(io::ErrorKind::InvalidData.into()),
                    None if update.pending => Ok(None),
                    None => {
                        let mut finished = current.clone();
                        finished.refinement_pending = false;
                        finished.refinement_outcome = update.refinement_outcome;
                        Ok(Some(finished))
                    }
                }
            }
            Ok(_) => Err(io::ErrorKind::InvalidData.into()),
            Err(error) => Err(error),
        };
        if result.is_err() {
            // A timeout can leave a partial frame. Never reuse an uncertain
            // transport; the displayed composition is held for the next key.
            self.detach();
        }
        result
    }
    /// Commit visible composition before a native input-source change.
    /// An uncertain document edit must keep the current source and block retries.
    pub fn commit_for_source_change(
        &mut self,
        context: Context,
        document: &mut impl Document,
    ) -> bool {
        if context.privacy == Privacy::Password as i32 {
            self.key(Key::default(), context, true, document);
            return true;
        }
        if self.blocked {
            return false;
        }
        if self.last_reading.is_empty() && !self.policy.composing {
            return true;
        }
        self.key(
            Key {
                special: SpecialKey::Enter as i32,
                ..Default::default()
            },
            context,
            false,
            document,
        );
        // Leaving the source is the user's request to end the composition.
        // If the engine could not commit it, its reading is inserted.
        if self.held {
            self.finish(document);
        }
        !self.blocked && self.last_reading.is_empty() && !self.policy.composing
    }
    pub fn select(&mut self, handle: CandidateHandle, document: &mut impl Document) -> bool {
        if self.blocked || self.held {
            return true;
        }
        let Some(seq) = self.event_seq.checked_add(1) else {
            return self.recover();
        };
        self.event_seq = seq;
        let request = Request {
            body: Some(request::Body::Select(SelectRequest {
                revision: Some(self.revision),
                event_seq: seq,
                handle: Some(handle),
                submit: true,
            })),
        };
        self.request(request, seq, document)
            .unwrap_or(self.held || self.blocked)
    }
    fn matches(&self, reply: &KeyReply, seq: u64) -> bool {
        bounded_reply(reply)
            && reply.event_seq == seq
            && reply.revision.as_ref().is_some_and(|r| {
                r.connection == self.revision.connection
                    && r.context == self.revision.context
                    && r.composition >= self.revision.composition
            })
    }
    /// Applies a reply to the document and acknowledges the result.
    fn accept(
        &mut self,
        reply: &KeyReply,
        seq: u64,
        document: &mut (impl Document + ?Sized),
    ) -> bool {
        let applied = document.apply(reply);
        self.applied_seq = seq;
        if let Some(revision) = reply.revision {
            self.revision = revision;
        }
        let ack = Request {
            body: Some(request::Body::Ack(Ack {
                revision: Some(self.revision),
                event_seq: seq,
                applied,
            })),
        };
        if applied {
            self.last_reading = reply.preedit.as_ref().map(reading).unwrap_or_default();
            self.policy = reply.key_policy.unwrap_or_default();
        }
        if self
            .transport
            .as_mut()
            .is_none_or(|t| t.notify(&ack).is_err())
            || !applied
        {
            if applied {
                // The edit is in the document but the engine never learned
                // so. Its session ends; what is displayed is held.
                self.held = false;
                self.detach();
            } else {
                self.transport = None;
                self.blocked = true;
                record(Event::DocumentRejectedEdit);
            }
        }
        applied
    }
    fn request(
        &mut self,
        request: Request,
        seq: u64,
        document: &mut impl Document,
    ) -> Option<bool> {
        let response = self
            .transport
            .as_mut()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))
            .and_then(|t| t.exchange(&request));
        match response {
            Ok(Response {
                body: Some(response::Body::Key(reply)),
            }) if self.matches(&reply, seq) => {
                if seq <= self.applied_seq {
                    return Some(reply.consumed);
                }
                let applied = self.accept(&reply, seq, document);
                Some(reply.consumed || !applied)
            }
            other => {
                record(match &other {
                    Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                        Event::ExchangeTimedOut
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotConnected => {
                        Event::ConnectFailed
                    }
                    Err(_) => Event::ExchangeFailed,
                    Ok(Response {
                        body: Some(response::Body::Failure(failure)),
                    }) => match failure::Code::try_from(failure.code) {
                        Ok(failure::Code::Stale) => Event::EngineReportedStale,
                        Ok(failure::Code::Unavailable) => Event::EngineReportedUnavailable,
                        _ => Event::EngineRejectedRequest,
                    },
                    Ok(_) => Event::UnexpectedReply,
                });
                self.recover();
                None
            }
        }
    }
    /// Kept for frontends that replace the client instead of reattaching.
    pub fn can_reconnect(&self) -> bool {
        !self.blocked && self.last_reading.is_empty() && self.transport.is_none()
    }
    /// Whether an idle, disconnected client still allows conversion starters.
    pub fn can_start_offline(&self) -> bool {
        self.can_reconnect() && self.policy.enabled && !self.held
    }
    /// The focus, input source or document is going away: end the composition
    /// by inserting the last acknowledged reading. Returns whether the
    /// document is blocked by a rejected edit.
    pub fn finish(&mut self, document: &mut impl Document) -> bool {
        if self.blocked {
            return true;
        }
        self.transport = None;
        if self.last_reading.is_empty() {
            self.held = false;
            self.stranded = false;
            return false;
        }
        if document.finish_reading(&self.last_reading) {
            record(if self.held {
                Event::HeldReadingFinished
            } else {
                Event::ReadingFinishedByRequest
            });
            self.release();
            false
        } else {
            self.blocked = true;
            record(Event::DocumentRejectedEdit);
            true
        }
    }
    fn release(&mut self) {
        self.last_reading.clear();
        self.policy.composing = false;
        self.held = false;
        self.stranded = false;
        self.restore_failures = 0;
    }
    /// Drops the session. A displayed composition becomes a hold.
    fn detach(&mut self) {
        self.transport = None;
        if !self.last_reading.is_empty() && !self.held {
            self.held = true;
            self.stranded = false;
            self.restore_failures = 0;
            record(Event::CompositionHeld);
        }
    }
    /// An exchange failed or returned something else than its reply. The key
    /// is not sent again. With a composition displayed it is used up and the
    /// composition is held; otherwise the application receives the key.
    fn recover(&mut self) -> bool {
        if self.blocked {
            return true;
        }
        self.detach();
        self.held
    }
    fn resume(
        &mut self,
        key: &Key,
        context: &Context,
        passed_through: bool,
        document: &mut impl Document,
    ) -> Held {
        if key.modifiers == 0 && key.text.is_empty() {
            if key.special == SpecialKey::Enter as i32 {
                self.finish(document);
                return Held::Used;
            }
            if key.special == SpecialKey::Escape as i32 {
                if document.cancel() {
                    self.release();
                } else {
                    self.transport = None;
                    self.blocked = true;
                    record(Event::DocumentRejectedEdit);
                }
                return Held::Used;
            }
        }
        if passed_through {
            return Held::Passed;
        }
        if self.stranded || self.transport.is_none() {
            return Held::Used;
        }
        let Some(seq) = self.event_seq.checked_add(1) else {
            self.strand();
            return Held::Used;
        };
        if self.version < RESTORE_VERSION {
            // This engine cannot adopt a reading. Keep the connection for
            // the composition that follows the user's Enter or Escape.
            self.stranded = true;
            record(Event::RestoreFailed);
            return Held::Used;
        }
        let request = Request {
            body: Some(request::Body::Restore(RestoreRequest {
                revision: Some(self.revision),
                event_seq: seq,
                reading: self.last_reading.clone(),
                context: Some(context.clone()),
            })),
        };
        self.event_seq = seq;
        let response = self.transport.as_mut().map(|t| t.exchange(&request));
        match response {
            Some(Ok(Response {
                body: Some(response::Body::Key(reply)),
            })) if self.matches(&reply, seq)
                && reply.commit.is_none()
                && reply.candidates.is_none()
                && reply
                    .preedit
                    .as_ref()
                    .is_some_and(|preedit| reading(preedit) == self.last_reading) =>
            {
                self.held = false;
                if !self.accept(&reply, seq, document) || self.held {
                    return Held::Used;
                }
                record(Event::CompositionRestored);
                Held::Restored
            }
            // The exchange may only have been slow; a later key tries again.
            Some(Err(_)) => self.retry_later(),
            Some(Ok(Response {
                body: Some(response::Body::Failure(failure)),
            })) if [
                failure::Code::Stale as i32,
                failure::Code::Unavailable as i32,
            ]
            .contains(&failure.code) =>
            {
                self.retry_later()
            }
            // The engine refused this reading, or answered with another
            // composition. Asking again cannot help, and nothing is applied.
            _ => {
                self.strand();
                Held::Used
            }
        }
    }
    fn retry_later(&mut self) -> Held {
        self.transport = None;
        self.restore_failures += 1;
        if self.restore_failures >= RESTORE_ATTEMPTS {
            self.strand();
        } else {
            record(Event::RestoreFailed);
        }
        Held::Used
    }
    fn strand(&mut self) {
        self.transport = None;
        self.stranded = true;
        record(Event::RestoreFailed);
    }
}
impl<T> Drop for Client<T> {
    fn drop(&mut self) {
        self.last_reading.clear();
    }
}

fn bounded_reply(reply: &KeyReply) -> bool {
    if reply.preedit.as_ref().is_some_and(|p| {
        p.segments.len() > 256 || surface(p).len() > MAX_PREEDIT || reading(p).len() > MAX_PREEDIT
    }) || reply.commit.as_ref().is_some_and(|c| {
        c.value.len() > MAX_PREEDIT || c.reading.len() > MAX_PREEDIT || c.words.len() > 256
    }) {
        return false;
    }
    reply.candidates.as_ref().is_none_or(|page| {
        page.items.len() <= PAGE_SIZE
            && page.total <= 4096
            && page.items.iter().all(|c| {
                c.value.len() <= MAX_PREEDIT
                    && c.reading.len() <= MAX_PREEDIT
                    && c.annotation.len() <= 512
            })
    })
}

fn valid_refinement(page: &CandidatePage, current: &CandidatePage, connection: u64) -> bool {
    if page.refinement_pending
        || page.candidate_revision <= current.candidate_revision
        || page.total != current.total
        || page.start != current.start
        || page.selected != current.selected
        || page.font_size != current.font_size
        || page.footer != current.footer
        || page.items.len() != current.items.len()
        || page.items.len() > PAGE_SIZE
        || page.total > 4096
    {
        return false;
    }
    let mut before = Vec::new();
    let mut after = Vec::new();
    for (offset, (old, new)) in current.items.iter().zip(&page.items).enumerate() {
        let Some(slot) = page.start.checked_add(offset as u32) else {
            return false;
        };
        if slot >= page.total
            || new.number != offset as u32 + 1
            || new.value.len() > MAX_PREEDIT
            || new.reading.len() > MAX_PREEDIT
            || new.annotation.len() > 512
            || new.handle.is_none_or(|h| {
                h.session_epoch != connection
                    || h.candidate_revision != page.candidate_revision
                    || h.slot != slot
            })
            || ((slot == page.selected || old.source != CandidateSource::Mozc as i32)
                && (old.value != new.value
                    || old.reading != new.reading
                    || old.source != new.source))
        {
            return false;
        }
        before.push((&old.value, &old.reading, old.source));
        after.push((&new.value, &new.reading, new.source));
    }
    before.sort_unstable();
    after.sort_unstable();
    before == after
}
