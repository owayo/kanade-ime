#![allow(non_snake_case)]
use kanade_ipc::{
    self as ipc, Key, KeyReply, Preedit, Privacy, SpecialKey,
    frontend::{Client, Document},
    windows::Pipe,
};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    ffi::c_void,
    path::PathBuf,
    rc::{Rc, Weak},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::*,
        System::{
            Com::*,
            LibraryLoader::GetModuleFileNameW,
            Variant::{VARIANT, VT_BOOL, VT_EMPTY},
        },
        UI::{Input::KeyboardAndMouse::*, TextServices::*},
    },
    core::{BOOL, Error, GUID, HRESULT, IUnknown, Interface, Ref, Result, implement},
};
use windows_core::IUnknownImpl;
mod attributes;
mod candidates;
#[cfg(test)]
mod host;
#[cfg(test)]
type SessionPipe = host::TestPipe;
#[cfg(not(test))]
type SessionPipe = Pipe;
mod input_mode;
mod language_bar;
mod mode_hint;
mod mode_icon;
mod programs;
mod registration;
mod uiless;
use candidates::CandidateWindow;
const CLSID: GUID = GUID::from_u128(0x6dafa84e_c56a_42be_983e_0d18f3dc5b21);
const PROFILE: GUID = GUID::from_u128(0xa1c81465_d31b_41d4_b7c4_35641955f560);
static MODULE: AtomicUsize = AtomicUsize::new(0);
static OBJECTS: AtomicUsize = AtomicUsize::new(0);
static LOCKS: AtomicUsize = AtomicUsize::new(0);
struct ModuleGuard;
impl ModuleGuard {
    fn new() -> Self {
        OBJECTS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}
impl Drop for ModuleGuard {
    fn drop(&mut self) {
        OBJECTS.fetch_sub(1, Ordering::AcqRel);
    }
}
fn failure() -> Error {
    E_FAIL.into()
}
fn module_path() -> Result<PathBuf> {
    let mut buffer = [0u16; 32768];
    let len = unsafe {
        GetModuleFileNameW(
            Some(HMODULE(MODULE.load(Ordering::Acquire) as *mut c_void)),
            &mut buffer,
        )
    } as usize;
    let value = buffer
        .get(..len)
        .filter(|_| len > 0 && len < 32768)
        .ok_or_else(failure)?;
    Ok(PathBuf::from(
        String::from_utf16(value).map_err(|_| failure())?,
    ))
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PhysicalKey {
    vk: u8,
    // Bits 16..=24 include the scan code and the extended-key bit.
    scan: u16,
}

impl PhysicalKey {
    fn new(w: WPARAM, l: LPARAM) -> Option<Self> {
        Some(Self {
            vk: w.0.try_into().ok()?,
            scan: ((l.0 >> 16) & 0x1ff) as u16,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DownRoute {
    NoDown,
    Tip,
    Application,
}

#[derive(Default)]
struct Presses(BTreeMap<PhysicalKey, Rc<Cell<DownRoute>>>);

impl Presses {
    // A callback may release or replace this press before Down returns. Its
    // result belongs to this cell even if the map now holds a newer press.
    fn begin(&mut self, w: WPARAM, l: LPARAM) -> Option<Rc<Cell<DownRoute>>> {
        let key = PhysicalKey::new(w, l)?;
        let repeat = l.0 & (1 << 30) != 0;
        self.0.retain(|id, _| {
            // WPF omits the scan code and repeat bit for character keys.
            let fresh = *id == key && key.scan != 0 && !repeat;
            !fresh && unsafe { GetKeyState(i32::from(id.vk)) < 0 }
        });
        let press = self.0.entry(key).or_insert_with(|| {
            // A repeat can be the first Down seen by a newly attached sink.
            Rc::new(Cell::new(if repeat {
                DownRoute::Application
            } else {
                DownRoute::NoDown
            }))
        });
        Some(press.clone())
    }

    fn up(&mut self, w: WPARAM, l: LPARAM, delivered: bool) -> bool {
        let Some(key) = PhysicalKey::new(w, l) else {
            return false;
        };
        let taken = self.0.get(&key).is_some_and(|p| p.get() == DownRoute::Tip);
        // TSF may omit KeyUp when TestKeyUp passes it to the application.
        if delivered || !taken {
            self.0.remove(&key);
        }
        taken
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct KeyMessage {
    time: i32,
    vk: usize,
    flags: isize,
    context: usize,
}

impl KeyMessage {
    fn current(context: &ITfContext, w: WPARAM, l: LPARAM) -> Self {
        Self {
            time: unsafe { windows::Win32::UI::WindowsAndMessaging::GetMessageTime() },
            vk: w.0,
            flags: l.0,
            context: context.as_raw() as usize,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyTest {
    Passed,
    Engine,
    ConnectedStarter,
    OfflineStarter { handshake_failed: bool },
}

#[derive(Clone, Copy)]
struct TestedKey {
    message: KeyMessage,
    decision: KeyTest,
    field: FieldScope,
}

#[derive(PartialEq, Eq)]
enum Connection {
    Unchanged,
    Attached,
    Unavailable,
    HandshakeFailed,
}

// One synchronous client operation, including the TSF callbacks and the ACK.
struct Editing {
    context: ITfContext,
    destination: RefCell<Option<ITfDocumentMgr>>,
    manager: RefCell<Option<ITfDocumentMgr>>,
    policy: Cell<ipc::KeyPolicySummary>,
    field: FieldScope,
    app_privacy: Privacy,
    focus_epoch: u64,
    leaving: Cell<bool>,
    terminated: Cell<bool>,
}

impl Editing {
    fn active(&self) -> bool {
        !self.leaving.get() && !self.terminated.get()
    }
    fn owns(&self, pending: &Pending) -> bool {
        pending.focus_epoch == self.focus_epoch
            && pending.action.context().is_none_or(|c| c == &self.context)
    }
}

// A claimed key has not been sent to the engine yet. Its field and focus
// lifetime travel with it, even when the current client has to be replaced.
struct Pending {
    focus_epoch: u64,
    input_rect: RECT,
    action: Action,
}

enum Delivery {
    Passed,
    Down,
    Starter,
    Queued,
}

enum Action {
    Key {
        context: ITfContext,
        key: Key,
        starter: bool,
        field: FieldScope,
    },
    Mode {
        context: Option<ITfContext>,
        tip: windows_core::ComObject<Tip>,
        command: input_mode::Command,
    },
    Choose {
        context: ITfContext,
        handle: ipc::CandidateHandle,
    },
    Poll {
        context: ITfContext,
        revision: u64,
        expired: bool,
    },
}

impl Action {
    fn context(&self) -> Option<&ITfContext> {
        match self {
            Self::Key { context, .. }
            | Self::Choose { context, .. }
            | Self::Poll { context, .. } => Some(context),
            Self::Mode { context, .. } => context.as_ref(),
        }
    }
}

struct Shared {
    open_pipe: fn(bool) -> Option<Pipe>,
    observed: RefCell<ipc::Context>,
    attribute: Cell<u32>,
    ui_manager: RefCell<Option<ITfUIElementMgr>>,
    ui_element: RefCell<Option<(u32, ITfUIElement)>>,
    page: RefCell<ipc::CandidatePage>,
    ui_native: Cell<bool>,
    ui_selection: Cell<u32>,
    tested: Cell<Option<TestedKey>>,
    client: RefCell<Option<Client<SessionPipe>>>,
    editing: RefCell<Option<Rc<Editing>>>,
    departing: RefCell<Option<Rc<Editing>>>,
    pending: RefCell<VecDeque<Pending>>,
    // Scope and focus queries while delivering a starter can reenter before
    // its reply supplies the new key policy.
    dispatching_starter: RefCell<Option<(u64, ITfContext)>>,
    draining: Cell<bool>,
    mode_queued: Cell<bool>,
    japanese: Cell<bool>,
    focus_epoch: Cell<u64>,
    show_mode_hint: Cell<bool>,
    composition: RefCell<Option<ITfComposition>>,
    composition_generation: Cell<u64>,
    starting_composition: Cell<bool>,
    context: RefCell<Option<ITfContext>>,
    manager: RefCell<Option<ITfThreadMgr>>,
    tid: Cell<u32>,
    field: Cell<FieldScope>,
    secure: Cell<bool>,
    preedit: RefCell<Preedit>,
    window: RefCell<Option<CandidateWindow>>,
    mode_hint: RefCell<Option<mode_hint::Hint>>,
    caret: Cell<RECT>,
    owner: Cell<HWND>,
}
impl Shared {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            open_pipe: programs::engine,
            observed: RefCell::new(ipc::Context::default()),
            attribute: Cell::new(0),
            ui_manager: RefCell::new(None),
            ui_element: RefCell::new(None),
            page: RefCell::new(ipc::CandidatePage::default()),
            ui_native: Cell::new(true),
            ui_selection: Cell::new(0),
            tested: Cell::new(None),
            client: RefCell::new(None),
            editing: RefCell::new(None),
            departing: RefCell::new(None),
            pending: RefCell::new(VecDeque::new()),
            dispatching_starter: RefCell::new(None),
            draining: Cell::new(false),
            mode_queued: Cell::new(false),
            japanese: Cell::new(false),
            focus_epoch: Cell::new(0),
            show_mode_hint: Cell::new(true),
            composition: RefCell::new(None),
            composition_generation: Cell::new(0),
            starting_composition: Cell::new(false),
            context: RefCell::new(None),
            manager: RefCell::new(None),
            tid: Cell::new(0),
            field: Cell::new(FieldScope::Bypass),
            secure: Cell::new(false),
            preedit: RefCell::new(Preedit::default()),
            window: RefCell::new(None),
            mode_hint: RefCell::new(None),
            caret: Cell::new(RECT::default()),
            owner: Cell::new(HWND::default()),
        })
    }
    /// Gives the client a connection when it has none. An existing client is
    /// kept: it may hold a composition that the new session will restore.
    fn connect(&self, allow_start: bool) -> Connection {
        if self.editing.borrow().is_some() {
            return Connection::Unchanged;
        }
        let Ok(mut slot) = self.client.try_borrow_mut() else {
            return Connection::Unavailable;
        };
        if slot.as_mut().is_some_and(|c| !c.needs_transport()) {
            return Connection::Unchanged;
        }
        let allow_start = allow_start && slot.as_ref().is_none_or(Client::can_start_offline);
        let Some(pipe) = (self.open_pipe)(allow_start) else {
            return Connection::Unavailable;
        };
        #[cfg(test)]
        let pipe = host::TestPipe::new(pipe);
        let result = match slot.as_mut() {
            Some(client) => client.reattach(pipe),
            None => Client::connect(pipe).map(|client| *slot = Some(client)),
        };
        if result.is_ok() {
            Connection::Attached
        } else {
            Connection::HandshakeFailed
        }
    }
    fn with_client<R>(
        self: &Rc<Self>,
        context: &ITfContext,
        operation: impl FnOnce(&mut Client<SessionPipe>, &mut NativeDocument<'_>) -> R,
    ) -> Option<R> {
        if self.editing.borrow().is_some() {
            return None;
        }
        let mut client = self.client.try_borrow_mut().ok()?.take()?;
        let departure = self.departing.borrow().clone();
        let editing = Rc::new(Editing {
            context: context.clone(),
            destination: RefCell::new(
                departure
                    .as_ref()
                    .and_then(|e| e.destination.borrow().clone()),
            ),
            manager: RefCell::new(None),
            policy: Cell::new(client.key_policy()),
            field: self.field.get(),
            app_privacy: client.privacy_for_app(&self.observed.borrow().app),
            focus_epoch: departure
                .as_ref()
                .map_or(self.focus_epoch.get(), |e| e.focus_epoch),
            leaving: Cell::new(departure.is_some()),
            terminated: Cell::new(false),
        });
        *self.editing.borrow_mut() = Some(editing.clone());
        self.context.replace(Some(context.clone()));
        let manager = match departure {
            Some(e) => e.manager.borrow().clone(),
            None => unsafe { context.GetDocumentMgr().ok() },
        };
        editing.manager.replace(manager);
        let mut doc = NativeDocument {
            accept_displayed: false,
            editing: &editing,
            shared: self.clone(),
        };
        let result = operation(&mut client, &mut doc);
        if !editing.leaving.get() && !self.pending.borrow().is_empty() && !self.focused(context) {
            editing.leaving.set(true);
        }
        if editing.terminated.get() || client.edit_rejected() {
            drop(client);
            self.clear_composition();
        } else {
            self.client.replace(Some(client));
        }
        if !self.japanese.get() {
            self.finish_document(&editing);
        }
        if editing.leaving.get() {
            self.departing.replace(Some(editing.clone()));
        }
        self.editing.borrow_mut().take();
        self.drain();
        Some(result)
    }

    fn finish_document(self: &Rc<Self>, editing: &Rc<Editing>) {
        self.editing.replace(Some(editing.clone()));
        let client = self.client.borrow_mut().take();
        if let Some(mut client) = client
            && !editing.terminated.get()
            && editing.attached()
        {
            let mut doc = NativeDocument {
                accept_displayed: false,
                editing,
                shared: self.clone(),
            };
            refresh_scope(self, &editing.context, editing.field, false);
            if editing.attached() {
                client.finish(&mut doc);
            }
        }
        self.clear_document();
        self.editing.take();
    }

    fn active_edit(&self) -> Option<Rc<Editing>> {
        self.editing
            .borrow()
            .clone()
            .or_else(|| self.departing.borrow().clone())
    }

    fn busy(&self) -> bool {
        self.draining.get() || self.editing.borrow().is_some()
    }

    fn queue_epoch(&self, context: Option<&ITfContext>) -> u64 {
        self.active_edit()
            .filter(|e| {
                e.leaving.get()
                    && Some(&e.context) == context
                    && (e.destination.borrow().is_none()
                        || *e.destination.borrow() != *e.manager.borrow())
            })
            .map_or(self.focus_epoch.get(), |e| e.focus_epoch)
    }

    fn has_queued_starter(&self, context: &ITfContext) -> bool {
        let epoch = self.queue_epoch(Some(context));
        self.dispatching_starter.borrow().as_ref()
            .is_some_and(|(e, c)| *e == epoch && c == context)
            || self.pending.borrow().iter().any(|p| {
                p.focus_epoch == epoch
                    && matches!(&p.action, Action::Key { context: c, starter: true, .. } if c == context)
            })
    }

    fn enqueue(&self, action: Action) {
        if matches!(action, Action::Mode { .. }) {
            self.mode_queued.set(true);
        }
        let epoch = self.queue_epoch(action.context());
        let input_rect = mode_hint::input_rect();
        self.pending.borrow_mut().push_back(Pending {
            focus_epoch: epoch,
            input_rect,
            action,
        });
    }

    fn focused(&self, context: &ITfContext) -> bool {
        let manager = self.manager.borrow().clone();
        unsafe {
            context.GetDocumentMgr().is_ok_and(|document| {
                document.GetTop().as_ref() == Ok(context)
                    && manager.is_none_or(|m| m.GetFocus().as_ref() == Ok(&document))
            })
        }
    }

    fn deliverable(&self, context: Option<&ITfContext>, epoch: u64) -> bool {
        let departure = self.departing.borrow().clone();
        match departure {
            Some(e) => {
                e.focus_epoch == epoch && context.is_none_or(|c| c == &e.context) && e.attached()
            }
            None => self.focus_epoch.get() == epoch && context.is_none_or(|c| self.focused(c)),
        }
    }

    fn drain(self: &Rc<Self>) {
        if self.draining.replace(true) {
            return;
        }
        let mut discard_location = None;
        loop {
            self.dispatching_starter.take();
            let departure = self.departing.borrow().clone();
            let next = if let Some(e) = &departure {
                // Keep the departing session until its last queued action has
                // been acknowledged. New focus epochs wait for their own client.
                let mut queue = self.pending.borrow_mut();
                let next = queue
                    .iter()
                    .position(|p| e.owns(p))
                    .and_then(|i| queue.remove(i));
                drop(queue);
                if next.is_none() {
                    self.finish_document(e);
                    if !self.pending.borrow().iter().any(|p| e.owns(p)) {
                        self.departing.take();
                    }
                    continue;
                }
                next
            } else {
                self.pending.borrow_mut().pop_front()
            };
            let Some(pending) = next else {
                self.mode_queued.set(false);
                if let Some(input_rect) = discard_location.take() {
                    // Showing the hint can itself call back into the key sink.
                    self.report_discard(input_rect);
                    continue;
                }
                break;
            };
            let context = pending.action.context();
            if let Action::Key {
                context,
                starter: true,
                ..
            } = &pending.action
            {
                self.dispatching_starter
                    .replace(Some((pending.focus_epoch, context.clone())));
            }
            if !self.deliverable(context, pending.focus_epoch) {
                if !matches!(pending.action, Action::Poll { .. }) {
                    discard_location.get_or_insert(pending.input_rect);
                }
                continue;
            }
            if let Action::Key { context, .. } = &pending.action
                && self.context.borrow().as_ref().is_some_and(|c| c != context)
            {
                self.pending.borrow_mut().push_front(pending);
                self.end_session();
                continue;
            }
            let applied = match pending.action {
                Action::Mode {
                    tip,
                    command,
                    context,
                } => tip.apply_input_mode(command, context.as_ref()).is_ok(),
                Action::Key {
                    context,
                    key,
                    starter,
                    field,
                } => {
                    self.japanese.get()
                        && self.queued_key(&context, pending.focus_epoch, key, starter, field)
                }
                Action::Choose { context, handle } => {
                    self.context.borrow().as_ref() == Some(&context)
                        && self
                            .page
                            .borrow()
                            .items
                            .iter()
                            .any(|c| c.handle == Some(handle))
                        && self.choose_now(handle)
                }
                Action::Poll {
                    context,
                    revision,
                    expired,
                } => {
                    if departure.is_none()
                        && self.context.borrow().as_ref() == Some(&context)
                        && self.page.borrow().candidate_revision == revision
                    {
                        self.poll_now(expired);
                    }
                    true
                }
            };
            if !applied {
                discard_location.get_or_insert(pending.input_rect);
            }
        }
        self.draining.set(false);
    }

    fn queued_key(
        self: &Rc<Self>,
        context: &ITfContext,
        epoch: u64,
        key: Key,
        starter: bool,
        limit: FieldScope,
    ) -> bool {
        // A refused read lock must not tear down an acknowledged mark. Check
        // scope before connecting, then use the current Welcome to read context.
        let previous = self.field.get();
        let Some(field) = try_refresh_scope(self, context, limit, false) else {
            self.field.set(previous);
            return false;
        };
        if !self.deliverable(Some(context), epoch) {
            return false;
        }
        if field == FieldScope::Bypass {
            self.suspend_secure();
            return false;
        }
        self.connect(starter && self.outage_scope());
        if self.can_start_offline() {
            if !self.outage_scope() || self.departing.borrow().is_some() {
                return false;
            }
            self.report_outage(context);
            return true;
        }
        let Some(field) = try_refresh_scope(self, context, field, true) else {
            self.field.set(field);
            return false;
        };
        if !self.deliverable(Some(context), epoch) {
            return false;
        }
        if field == FieldScope::Bypass {
            self.suspend_secure();
            return false;
        }
        self.send_key(context, key, Delivery::Queued) == Some(true)
    }

    fn report_discard(&self, input_rect: RECT) {
        if self.show_mode_hint.get()
            && self.ui_native.get()
            && let Ok(hint) = mode_hint::Hint::for_discard(input_rect)
        {
            self.mode_hint.replace(Some(hint));
        }
    }

    fn composition_terminated(&self, composition: Ref<ITfComposition>, generation: Option<u64>) {
        let Some(ended) = composition.as_ref() else {
            return;
        };
        if generation.is_some_and(|g| g != self.composition_generation.get())
            || (self.composition.borrow().as_ref() != Some(ended)
                && !(generation.is_some() && self.starting_composition.get()))
        {
            return;
        }
        // The host already decided what stays in the document.
        if let Some(editing) = self.editing.borrow().as_ref() {
            editing.terminated.set(true);
            return;
        }
        self.client.borrow_mut().take();
        self.clear_composition();
    }
    fn composition_was_terminated(&self) -> bool {
        self.editing
            .borrow()
            .as_ref()
            .is_some_and(|editing| editing.terminated.get())
    }
    fn clear_composition(&self) {
        self.composition.borrow_mut().take();
        *self.preedit.borrow_mut() = Preedit::default();
        self.hide();
    }
    fn outage_scope(&self) -> bool {
        self.field.get() != FieldScope::Bypass && !self.secure.get()
    }
    /// Only an idle, disconnected session can drop a starter.
    fn can_start_offline(&self) -> bool {
        self.editing.borrow().is_none()
            && self
                .client
                .try_borrow()
                .is_ok_and(|slot| slot.as_ref().is_none_or(Client::can_start_offline))
    }
    fn hide(&self) {
        *self.page.borrow_mut() = ipc::CandidatePage::default();
        self.stop_refinement();
        let element = self
            .ui_element
            .try_borrow_mut()
            .ok()
            .and_then(|mut e| e.take());
        if let Some((id, _)) = element
            && let Ok(manager) = self.ui_manager.try_borrow()
            && let Some(manager) = manager.as_ref()
        {
            let _ = unsafe { manager.EndUIElement(id) };
        }
        if let Ok(window) = self.window.try_borrow()
            && let Some(window) = window.as_ref()
        {
            window.hide();
        }
    }
    fn reset(self: &Rc<Self>) {
        self.focus_epoch.set(self.focus_epoch.get().wrapping_add(1));
        if let Some(edit) = self.active_edit() {
            edit.destination.take();
        }
        self.end_session();
    }

    fn end_session(self: &Rc<Self>) {
        if let Some(editing) = self.active_edit() {
            editing.leaving.set(true);
            return;
        }
        let context = self.context.borrow().clone();
        if let Some(context) = context
            && self
                .with_client(&context, |_, doc| doc.editing.leaving.set(true))
                .is_some()
        {
            return;
        }
        if let Ok(mut client) = self.client.try_borrow_mut() {
            client.take();
        }
        self.clear_document();
    }
    fn clear_document(&self) {
        if let Ok(mut hint) = self.mode_hint.try_borrow_mut() {
            hint.take();
        }
        self.hide();
        if let Ok(mut c) = self.composition.try_borrow_mut() {
            c.take();
        }
        if let Ok(mut p) = self.preedit.try_borrow_mut() {
            *p = Preedit::default();
        }
        self.tested.set(None);
        if let Ok(mut context) = self.context.try_borrow_mut() {
            context.take();
        }
        if let Ok(mut observed) = self.observed.try_borrow_mut() {
            *observed = ipc::Context::default();
        }
    }
    fn suspend_secure(&self) {
        self.tested.set(None);
        // A callback can arrive while either slot is borrowed. Leave a borrowed
        // slot to its holder, as before, rather than panic inside the host.
        let composition = self
            .composition
            .try_borrow_mut()
            .ok()
            .and_then(|mut c| c.take());
        let context = self
            .context
            .try_borrow_mut()
            .ok()
            .and_then(|mut c| c.take());
        if let Ok(mut client) = self.client.try_borrow_mut() {
            client.take();
        }
        if let Ok(mut preedit) = self.preedit.try_borrow_mut() {
            *preedit = Preedit::default();
        }
        if let Ok(mut observed) = self.observed.try_borrow_mut() {
            *observed = ipc::Context::default();
        }
        self.hide();
        if let (Some(context), Some(composition)) = (context, composition) {
            EndEdit::request(&context, self.tid.get(), composition, None);
        }
    }
    fn show(self: &Rc<Self>, page: &ipc::CandidatePage) {
        if let Ok(mut current) = self.page.try_borrow_mut() {
            *current = page.clone();
        }
        self.ui_selection
            .set(page.selected.saturating_sub(page.start));
        if let Ok(manager) = self.ui_manager.try_borrow()
            && let Some(manager) = manager.as_ref()
        {
            let current = self
                .ui_element
                .try_borrow()
                .ok()
                .and_then(|e| e.as_ref().map(|e| e.0));
            if let Some(id) = current {
                let _ = unsafe { manager.UpdateUIElement(id) };
            } else {
                let candidate: ITfCandidateListUIElementBehavior =
                    uiless::Element::new(self).into();
                let element: ITfUIElement = candidate.into();
                let mut native = BOOL::from(true);
                let mut id = 0;
                if unsafe { manager.BeginUIElement(&element, &mut native, &mut id) }.is_ok() {
                    self.ui_native.set(native.as_bool());
                    if let Ok(mut stored) = self.ui_element.try_borrow_mut() {
                        *stored = Some((id, element));
                    }
                }
            }
        }
        if !self.ui_native.get() && !page.refinement_pending {
            if let Ok(window) = self.window.try_borrow()
                && let Some(window) = window.as_ref()
            {
                window.stop_refinement();
                window.hide();
            }
            return;
        }
        if let Ok(mut window) = self.window.try_borrow_mut() {
            if window
                .as_ref()
                .is_some_and(|w| !w.owned_by(self.owner.get()))
            {
                window.take();
            }
            if window.is_none() {
                let weak = Rc::downgrade(self);
                let polling = weak.clone();
                *window = CandidateWindow::new(
                    self.owner.get(),
                    move |handle| {
                        if let Some(s) = weak.upgrade() {
                            s.choose(handle);
                        }
                    },
                    move |expired| {
                        if let Some(s) = polling.upgrade() {
                            s.poll_refinement(expired);
                        }
                    },
                )
                .ok();
            }
            if let Some(window) = window.as_mut() {
                if self.ui_native.get() {
                    let _ = window.show(page, self.caret.get());
                } else {
                    window.hide();
                }
                window.schedule_refinement(page.refinement_pending);
            }
        }
    }
    fn stop_refinement(&self) {
        if let Ok(window) = self.window.try_borrow()
            && let Some(window) = window.as_ref()
        {
            window.stop_refinement();
        }
    }
    fn poll_refinement(self: &Rc<Self>, expired: bool) {
        if self.busy() {
            let revision = self.page.borrow().candidate_revision;
            if let Some(context) = self.context.borrow().clone() {
                self.enqueue(Action::Poll {
                    context,
                    revision,
                    expired,
                });
            }
        } else {
            self.poll_now(expired);
        }
    }

    fn poll_now(self: &Rc<Self>, expired: bool) {
        let context = self.context.try_borrow().ok().and_then(|c| c.clone());
        let focused = self
            .manager
            .try_borrow()
            .ok()
            .and_then(|m| m.clone())
            .and_then(|m| unsafe { m.GetFocus().ok() })
            .and_then(|d| unsafe { d.GetTop().ok() });
        let Some(context) = context.filter(|c| Some(c) == focused.as_ref()) else {
            self.hide();
            return;
        };
        // Query input scope only: candidate polling never reads surrounding text.
        let epoch = self.focus_epoch.get();
        let previous = self.field.get();
        let Some(field) = try_refresh_scope(self, &context, FieldScope::Ordinary, false) else {
            self.field.set(previous);
            return;
        };
        if epoch != self.focus_epoch.get() {
            return;
        }
        if field == FieldScope::Bypass {
            self.suspend_secure();
            return;
        }
        if !self
            .observed
            .try_borrow()
            .is_ok_and(|c| c.privacy == Privacy::Normal as i32)
        {
            self.hide();
            return;
        }
        let page = self.page.try_borrow().ok().map(|p| p.clone());
        let Some(mut page) = page.filter(|p| p.refinement_pending) else {
            self.stop_refinement();
            return;
        };
        if self
            .with_client(&context, |client, doc| {
                match client.poll_with_preview(&page, doc) {
                    Ok(Some(page)) => self.show(&page),
                    Ok(None) if expired => {
                        page.mark_refinement_timeout();
                        self.show(&page);
                    }
                    Ok(None) => {}
                    Err(_) => self.hide(),
                }
            })
            .is_none()
            && expired
        {
            page.mark_refinement_timeout();
            self.show(&page);
        }
    }
    fn choose(self: &Rc<Self>, handle: ipc::CandidateHandle) {
        if self.busy() {
            let context = self.context.borrow().clone();
            if let Some(context) = context {
                self.enqueue(Action::Choose { context, handle });
            } else {
                self.report_discard(mode_hint::input_rect());
            }
        } else if !self.choose_now(handle) {
            self.report_discard(mode_hint::input_rect());
        }
    }

    fn choose_now(self: &Rc<Self>, handle: ipc::CandidateHandle) -> bool {
        self.stop_refinement();
        let context = self.context.try_borrow().ok().and_then(|c| c.clone());
        let Some(context) = context else {
            self.suspend_secure();
            return false;
        };
        let epoch = self
            .departing
            .borrow()
            .as_ref()
            .map_or(self.focus_epoch.get(), |e| e.focus_epoch);
        let previous = self.field.get();
        let Some(field) = try_refresh_scope(self, &context, FieldScope::Ordinary, false) else {
            self.field.set(previous);
            return false;
        };
        if !self.deliverable(Some(&context), epoch) {
            return false;
        }
        if field == FieldScope::Bypass {
            self.suspend_secure();
            return false;
        }
        self.with_client(&context, |client, doc| {
            client.is_connected()
                && !client.is_held()
                && client.select(handle, doc)
                && !client.edit_rejected()
                && !client.is_held()
        }) == Some(true)
    }
}
#[implement(
    ITfTextInputProcessorEx,
    ITfKeyEventSink,
    ITfCompartmentEventSink,
    ITfThreadMgrEventSink,
    ITfCompositionSink,
    ITfDisplayAttributeProvider
)]
struct Tip {
    shared: Rc<Shared>,
    presses: RefCell<Presses>,
    sink_cookie: Cell<Option<u32>>,
    mode: RefCell<Option<input_mode::Mode>>,
    button: RefCell<Option<language_bar::Item>>,
    _guard: ModuleGuard,
}
impl Tip {
    fn new() -> Self {
        Self {
            shared: Shared::new(),
            presses: RefCell::default(),
            sink_cookie: Cell::new(None),
            mode: RefCell::new(None),
            button: RefCell::new(None),
            _guard: ModuleGuard::new(),
        }
    }
    fn sync_mode(&self) {
        if let Some(mode) = self.mode.borrow().as_ref() {
            let japanese = mode.japanese();
            if self.shared.japanese.replace(japanese)
                && !japanese
                && self.shared.editing.borrow().is_none()
            {
                // Finish at the OFF command, even if an ON follows in the same queue.
                let context = self.shared.context.borrow().clone();
                if let Some(context) = context {
                    self.shared.with_client(&context, |_, _| ());
                }
            }
        }
        self.sync_button();
    }
    fn apply_input_mode(
        &self,
        command: input_mode::Command,
        context: Option<&ITfContext>,
    ) -> Result<()> {
        if let Some(mode) = self.mode.borrow().as_ref() {
            mode.apply(self.shared.tid.get(), command)?;
        }
        self.sync_mode();
        if self.shared.show_mode_hint.get()
            && let Some(context) = context
        {
            let hint = mode_hint::Hint::at_selection(
                context,
                self.shared.tid.get(),
                self.shared.japanese.get(),
            );
            if let Ok(mut previous) = self.shared.mode_hint.try_borrow_mut() {
                *previous = hint.ok();
            }
        }
        Ok(())
    }
    /// The taskbar button, when this thread has one. The indicator calls
    /// back into the button, so the caller holds no borrow of the slot.
    fn taskbar_button(&self) -> Option<windows_core::ComObject<language_bar::ModeButton>> {
        self.button
            .try_borrow()
            .ok()
            .and_then(|button| button.as_ref().map(language_bar::Item::button))
    }
    /// Lets the taskbar button show the current mode and taskbar colours.
    fn sync_button(&self) {
        if let Some(button) = self.taskbar_button() {
            button.sync();
        }
    }
    /// Lets the button notice a taskbar theme that changed while this thread
    /// kept the focus. Called for every key, so the button paces itself.
    fn refresh_button(&self) {
        if let Some(button) = self.taskbar_button() {
            button.refresh();
        }
    }
}
impl ITfTextInputProcessor_Impl for Tip_Impl {
    fn Activate(&self, manager: Ref<ITfThreadMgr>, tid: u32) -> Result<()> {
        self.ActivateEx(manager, tid, 0)
    }
    fn Deactivate(&self) -> Result<()> {
        let button = self
            .button
            .try_borrow_mut()
            .ok()
            .and_then(|mut button| button.take());
        drop(button);
        let mode = self.mode.borrow_mut().take();
        drop(mode);
        self.shared.reset();
        if let Some(manager) = self
            .shared
            .manager
            .try_borrow_mut()
            .ok()
            .and_then(|mut m| m.take())
        {
            if let Ok(keys) = manager.cast::<ITfKeystrokeMgr>() {
                let _ = unsafe { keys.UnadviseKeyEventSink(self.shared.tid.get()) };
            }
            if let (Ok(source), Some(cookie)) =
                (manager.cast::<ITfSource>(), self.sink_cookie.take())
            {
                let _ = unsafe { source.UnadviseSink(cookie) };
            }
        }
        if let Ok(mut c) = self.shared.client.try_borrow_mut() {
            c.take();
        }
        self.presses.take();
        Ok(())
    }
}
impl ITfTextInputProcessorEx_Impl for Tip_Impl {
    fn ActivateEx(&self, manager: Ref<ITfThreadMgr>, tid: u32, flags: u32) -> Result<()> {
        let manager = manager.ok()?;
        self.shared.secure.set(flags & TF_TMAE_SECUREMODE != 0);
        self.shared
            .show_mode_hint
            .set(flags & (TF_TMAE_UIELEMENTENABLEDONLY | TF_TMAE_SECUREMODE) == 0);
        if let Ok(mut ui) = self.shared.ui_manager.try_borrow_mut() {
            *ui = manager.cast::<ITfUIElementMgr>().ok();
        }
        if let Ok(categories) = unsafe {
            CoCreateInstance::<_, ITfCategoryMgr>(&CLSID_TF_CategoryMgr, None, CLSCTX_INPROC_SERVER)
        } && let Ok(atom) = unsafe { categories.RegisterGUID(&attributes::ATTRIBUTE) }
        {
            self.shared.attribute.set(atom);
        }
        self.shared.tid.set(tid);
        if let Ok(mut slot) = self.shared.manager.try_borrow_mut() {
            *slot = Some(manager.clone());
        }
        let mode = input_mode::Mode::attach(manager, tid, &self.to_interface())?;
        // Input works without the taskbar button, so a host that refuses the
        // button still activates the text service.
        let button = language_bar::Item::add(
            manager,
            tid,
            mode.switch(),
            language_bar::Settings::of_host(flags & TF_TMAE_SECUREMODE != 0),
        )
        .ok();
        let keys = manager.cast::<ITfKeystrokeMgr>()?;
        let sink: ITfKeyEventSink = self.to_interface();
        unsafe {
            keys.AdviseKeyEventSink(tid, &sink, true)?;
        }
        let source = manager.cast::<ITfSource>()?;
        let sink: ITfThreadMgrEventSink = self.to_interface();
        let cookie =
            unsafe { source.AdviseSink(&ITfThreadMgrEventSink::IID, &sink) }.inspect_err(|_| {
                let _ = unsafe { keys.UnadviseKeyEventSink(tid) };
                self.presses.take();
            })?;
        self.sink_cookie.set(Some(cookie));
        self.shared.japanese.set(mode.japanese());
        *self.mode.borrow_mut() = Some(mode);
        if let Ok(mut slot) = self.button.try_borrow_mut() {
            *slot = button;
        }
        if self.shared.japanese.get() {
            self.shared.connect(false);
        }
        Ok(())
    }
}
impl ITfCompartmentEventSink_Impl for Tip_Impl {
    fn OnChange(&self, _: *const GUID) -> Result<()> {
        self.sync_mode();
        Ok(())
    }
}
impl ITfKeyEventSink_Impl for Tip_Impl {
    fn OnSetFocus(&self, foreground: BOOL) -> Result<()> {
        if foreground.as_bool() {
            if let Some(edit) = self.shared.active_edit().filter(|e| e.leaving.get()) {
                let manager = self.shared.manager.borrow().clone();
                edit.destination
                    .replace(manager.and_then(|m| unsafe { m.GetFocus().ok() }));
            }
            // The taskbar theme can change while another thread has the focus.
            self.sync_button();
        } else {
            self.shared.reset();
        }
        Ok(())
    }
    fn OnTestKeyDown(&self, context: Ref<ITfContext>, w: WPARAM, l: LPARAM) -> Result<BOOL> {
        let press = self.presses.borrow_mut().begin(w, l);
        let tested = self.test_down(context, w, l);
        if !matches!(tested, Ok(true))
            && let Some(press) = press
        {
            press.set(DownRoute::Application);
        }
        tested.map(BOOL::from)
    }
    fn OnKeyDown(&self, context: Ref<ITfContext>, w: WPARAM, l: LPARAM) -> Result<BOOL> {
        let press = self.presses.borrow_mut().begin(w, l);
        let consumed = self.key_down(context, w, l);
        if let Some(press) = press {
            if !matches!(consumed, Ok(true)) {
                press.set(DownRoute::Application);
            } else if press.get() == DownRoute::NoDown {
                press.set(DownRoute::Tip);
            }
        }
        consumed.map(BOOL::from)
    }
    fn OnTestKeyUp(&self, _: Ref<ITfContext>, w: WPARAM, l: LPARAM) -> Result<BOOL> {
        self.shared.tested.set(None);
        Ok(self.presses.borrow_mut().up(w, l, false).into())
    }
    fn OnKeyUp(&self, _: Ref<ITfContext>, w: WPARAM, l: LPARAM) -> Result<BOOL> {
        self.shared.tested.set(None);
        Ok(self.presses.borrow_mut().up(w, l, true).into())
    }
    fn OnPreservedKey(&self, context: Ref<ITfContext>, guid: *const GUID) -> Result<BOOL> {
        if let Some(command) = unsafe { guid.as_ref() }.and_then(input_mode::preserved) {
            self.set_input_mode(command, context.as_ref())?;
            return Ok(true.into());
        }
        Ok(false.into())
    }
}
impl Tip_Impl {
    fn test_down(&self, context: Ref<ITfContext>, w: WPARAM, l: LPARAM) -> Result<bool> {
        let previous = self.shared.tested.take();
        if !self.shared.busy() {
            self.refresh_button();
        }
        if input_mode::physical(w.0).is_some() {
            return Ok(true);
        }
        if !self.shared.japanese.get() && !self.shared.mode_queued.get() {
            return Ok(false);
        }
        let context = context.ok()?;
        let message = KeyMessage::current(context, w, l);
        if self.shared.busy() {
            let key = translate(w, l);
            let (owns, field) = key.as_ref().map_or((false, FieldScope::Bypass), |k| {
                self.nested_key(context, w, k)
            });
            self.shared.tested.set(Some(TestedKey {
                message,
                decision: if owns {
                    KeyTest::Engine
                } else {
                    KeyTest::Passed
                },
                field,
            }));
            return Ok(owns);
        }
        if let Some(tested) = previous.filter(|prior| prior.message == message) {
            self.shared.tested.set(Some(tested));
            return Ok(tested.decision != KeyTest::Passed);
        }
        let field = refresh_scope(&self.shared, context, FieldScope::Ordinary, false);
        if field == FieldScope::Bypass {
            self.shared.suspend_secure();
            self.shared.tested.set(Some(TestedKey {
                message,
                decision: KeyTest::Passed,
                field,
            }));
            return Ok(false);
        }
        let connection = self.shared.connect(false);
        let key = translate(w, l);
        let engine_owns = key.as_ref().is_some_and(|k| {
            // Keep keypad text and other host keys outside the TIP's keymap.
            (starter_virtual_key(w.0 as u32)
                || k.special != SpecialKey::None as i32
                || ipc::frontend::is_conversion_control(k))
                && self.shared.client.try_borrow().is_ok_and(|c| {
                    c.as_ref()
                        .is_some_and(|c| c.would_consume(k, Privacy::Unknown))
                })
        });
        let outage_starter =
            self.shared.outage_scope() && key.as_ref().is_some_and(|k| is_starter(w, k));
        let test = if engine_owns {
            if outage_starter {
                KeyTest::ConnectedStarter
            } else {
                KeyTest::Engine
            }
        } else if outage_starter && self.shared.can_start_offline() {
            KeyTest::OfflineStarter {
                handshake_failed: connection == Connection::HandshakeFailed,
            }
        } else {
            // An unconsumed key may never reach Down. Observe it once using
            // the connection and learning policy already checked here.
            if let Some(key) = key
                && !self.shared.can_start_offline()
            {
                if refresh_scope(&self.shared, context, field, true) == FieldScope::Bypass {
                    self.shared.suspend_secure();
                } else {
                    self.shared.send_key(context, key, Delivery::Passed);
                }
            }
            KeyTest::Passed
        };
        self.shared.tested.set(Some(TestedKey {
            message,
            decision: test,
            field,
        }));
        Ok(test != KeyTest::Passed)
    }
    fn key_down(&self, context: Ref<ITfContext>, w: WPARAM, l: LPARAM) -> Result<bool> {
        let previous = self.shared.tested.take();
        if !self.shared.busy() {
            self.refresh_button();
        }
        if let Some(command) = input_mode::physical(w.0) {
            if l.0 & (1 << 30) == 0 {
                self.set_input_mode(command, context.as_ref())?;
            }
            return Ok(true);
        }
        if !self.shared.japanese.get() && !self.shared.mode_queued.get() {
            return Ok(false);
        }
        let context = context.ok()?;
        let message = KeyMessage::current(context, w, l);
        let test = previous.filter(|prior| prior.message == message);
        if test.is_some_and(|key| key.decision == KeyTest::Passed) {
            return Ok(false);
        }
        let consumed = if self.shared.busy() {
            translate(w, l).is_some_and(|key| {
                let (owns, field) = self.nested_key(context, w, &key);
                if field == FieldScope::Bypass || (!owns && test.is_none()) {
                    return false;
                }
                self.shared.enqueue(Action::Key {
                    context: context.clone(),
                    starter: is_starter(w, &key),
                    key,
                    field: test.map_or(field, |t| t.field.min(field)),
                });
                true
            })
        } else {
            self.process_key(context, w, l, test)
        };
        Ok(consumed)
    }
    fn nested_key(&self, context: &ITfContext, w: WPARAM, key: &Key) -> (bool, FieldScope) {
        let same = self.shared.context.borrow().as_ref() == Some(context);
        let editing = self.shared.active_edit().filter(|e| e.context == *context);
        let field = if let Some(edit) = &editing {
            edit.field
        } else {
            // A different store may already grant reads. If it is still locked,
            // require a successful scope read at delivery.
            query_scope(
                None,
                self.shared.tid.get(),
                context,
                FieldScope::Ordinary,
                false,
            )
            .unwrap_or(FieldScope::Private)
        };
        let owns = if field == FieldScope::Bypass {
            false
        } else if self.shared.mode_queued.get() {
            true
        } else if self.shared.has_queued_starter(context) {
            ipc::KeyPolicySummary {
                enabled: true,
                composing: true,
                ..Default::default()
            }
            .would_consume(key)
        } else {
            match editing {
                Some(e) if e.active() => e.policy.get().would_consume(key),
                None if same => self
                    .shared
                    .client
                    .try_borrow()
                    .ok()
                    .and_then(|slot| {
                        slot.as_ref()
                            .map(|client| client.would_consume(key, Privacy::Unknown))
                    })
                    .unwrap_or_else(|| is_starter(w, key)),
                _ => is_starter(w, key),
            }
        };
        (owns, field)
    }

    fn set_input_mode(
        &self,
        command: input_mode::Command,
        context: Option<&ITfContext>,
    ) -> Result<()> {
        if self.shared.busy() {
            self.shared.enqueue(Action::Mode {
                context: context.cloned(),
                tip: self.to_object(),
                command,
            });
            Ok(())
        } else {
            self.apply_input_mode(command, context)
        }
    }

    fn process_key(
        &self,
        context: &ITfContext,
        w: WPARAM,
        l: LPARAM,
        tested: Option<TestedKey>,
    ) -> bool {
        let test = tested.map(|key| key.decision);
        let offline_claim = matches!(test, Some(KeyTest::OfflineStarter { .. }));
        let claimed = offline_claim || test == Some(KeyTest::ConnectedStarter);
        let refresh = |limit| match try_refresh_scope(&self.shared, context, limit, true) {
            Some(field) if field != FieldScope::Bypass => Ok(field),
            blocked => {
                self.shared.suspend_secure();
                // A confirmed bypass also releases a starter claimed during an outage.
                Err(blocked.is_none() && offline_claim)
            }
        };
        let field = match refresh(tested.map_or(FieldScope::Ordinary, |key| key.field)) {
            Ok(field) => field,
            Err(consumed) => return consumed,
        };
        if offline_claim && !self.shared.outage_scope() {
            return true;
        }
        let Some(key) = translate(w, l) else {
            return offline_claim;
        };
        let starter = is_starter(w, &key);
        if test
            == Some(KeyTest::OfflineStarter {
                handshake_failed: true,
            })
            && self.shared.can_start_offline()
        {
            // Test already waited for Hello. Check only for a missing pipe.
            drop((self.shared.open_pipe)(true));
            self.shared.report_outage(context);
            return true;
        }
        let connection = self.shared.connect(starter && self.shared.outage_scope());
        if connection == Connection::Attached {
            // The welcome supplied the app learning policy for this first key.
            if let Err(consumed) = refresh(field) {
                return consumed;
            }
            if offline_claim && !self.shared.outage_scope() {
                return true;
            }
        }
        let eligible = starter && self.shared.outage_scope();
        if self.shared.can_start_offline() {
            if eligible {
                self.shared.report_outage(context);
            }
            return claimed || eligible;
        }
        let delivery = if eligible {
            Delivery::Starter
        } else {
            Delivery::Down
        };
        let consumed = self
            .shared
            .send_key(context, key, delivery)
            .unwrap_or(claimed);
        consumed || offline_claim || (claimed && connection == Connection::Attached)
    }
}
impl Shared {
    fn send_key(
        self: &Rc<Self>,
        context: &ITfContext,
        key: Key,
        delivery: Delivery,
    ) -> Option<bool> {
        let observed = self.observed.try_borrow().ok()?.clone();
        self.with_client(context, |client, doc| {
            self.stop_refinement();
            let connected = client.is_connected();
            let claimed_in_queue = matches!(delivery, Delivery::Queued) && self.outage_scope();
            let drop_on_loss = claimed_in_queue
                || (matches!(delivery, Delivery::Starter)
                    && client.would_consume(&key, Privacy::Unknown));
            doc.accept_displayed = matches!(delivery, Delivery::Queued) && is_enter(&key);
            let result =
                client.key_result(key, observed, matches!(delivery, Delivery::Passed), doc);
            // Candidates of a lost session cannot be chosen any more.
            let held = client.is_held();
            if held {
                self.hide();
            }
            // KeyDown already returned TRUE for a queued key. A decline or
            // rejected document edit must reach the queue's discard notice.
            if matches!(delivery, Delivery::Queued)
                && (result == Some(false) || client.edit_rejected())
            {
                return Some(false);
            }
            if connected && client.is_connected() && result.is_some() && !held {
                // A handshake alone cannot tell whether the engine can process keys.
                if let Ok(mut hint) = self.mode_hint.try_borrow_mut() {
                    hint.take_if(|h| h.outage_shown_at.is_some());
                }
            } else if drop_on_loss && (result.is_none() && !held || claimed_in_queue && held) {
                // A missing pipe may need the supervisor. Leave the next Hello
                // for the next key, even if the stalled peer still accepts opens.
                drop((self.open_pipe)(true));
                if claimed_in_queue && doc.editing.leaving.get() {
                    return Some(false);
                }
                self.report_outage(context);
                return Some(true);
            }
            result.or(held.then_some(true))
        })?
    }
    fn report_outage(&self, context: &ITfContext) {
        const REPEAT_AFTER: Duration = Duration::from_secs(10);
        if !self.show_mode_hint.get()
            || self
                .mode_hint
                .try_borrow()
                .ok()
                .and_then(|hint| hint.as_ref()?.outage_shown_at)
                .is_some_and(|shown| shown.elapsed() < REPEAT_AFTER)
        {
            return;
        }
        let hint = mode_hint::Hint::for_outage(context, self.tid.get());
        if let Ok(hint) = hint
            && let Ok(mut previous) = self.mode_hint.try_borrow_mut()
        {
            *previous = Some(hint);
        }
    }
}
impl ITfThreadMgrEventSink_Impl for Tip_Impl {
    fn OnInitDocumentMgr(&self, _: Ref<ITfDocumentMgr>) -> Result<()> {
        Ok(())
    }
    fn OnUninitDocumentMgr(&self, document: Ref<ITfDocumentMgr>) -> Result<()> {
        self.reset_document(document.as_ref());
        Ok(())
    }
    fn OnSetFocus(
        &self,
        focused: Ref<ITfDocumentMgr>,
        previous: Ref<ITfDocumentMgr>,
    ) -> Result<()> {
        if let Some(edit) = self.shared.active_edit() {
            let current = if edit.leaving.get() {
                edit.destination.borrow().clone()
            } else {
                edit.manager.borrow().clone()
            };
            if focused.as_ref() != current.as_ref()
                && (previous.is_none()
                    || previous.as_ref() == current.as_ref()
                    || current.is_none())
            {
                self.shared.reset();
                edit.destination.replace(focused.cloned());
            }
        } else if previous.is_none()
            && let Some(focused) = focused.as_ref()
        {
            if self.shared.document_manager().as_ref() != Some(focused) {
                self.shared.reset();
            }
        } else if previous.as_ref() != focused.as_ref() {
            self.reset_document(previous.as_ref());
        }
        Ok(())
    }
    fn OnPushContext(&self, context: Ref<ITfContext>) -> Result<()> {
        let manager = context
            .as_ref()
            .and_then(|c| unsafe { c.GetDocumentMgr().ok() });
        self.reset_document(manager.as_ref());
        Ok(())
    }
    fn OnPopContext(&self, context: Ref<ITfContext>) -> Result<()> {
        let leaving = self.shared.active_edit().filter(|e| e.leaving.get());
        if let Some(edit) = leaving {
            let manager = context
                .as_ref()
                .and_then(|c| unsafe { c.GetDocumentMgr().ok() });
            if manager.is_some() && manager == *edit.destination.borrow() {
                self.shared.reset();
            }
        } else if self
            .shared
            .context
            .borrow()
            .as_ref()
            .is_none_or(|c| Some(c) == context.as_ref())
        {
            self.shared.reset();
        }
        Ok(())
    }
}
impl Tip_Impl {
    fn reset_document(&self, previous: Option<&ITfDocumentMgr>) {
        let leaving = self.shared.active_edit().filter(|e| e.leaving.get());
        if let Some(edit) = leaving {
            if previous.is_some() && edit.destination.borrow().as_ref() == previous {
                self.shared.reset();
            }
        } else if self.shared.context.borrow().is_none()
            || self.shared.document_manager().as_ref() == previous
        {
            self.shared.reset();
        }
    }
}
impl Shared {
    fn document_manager(&self) -> Option<ITfDocumentMgr> {
        let editing = self
            .editing
            .borrow()
            .as_ref()
            .and_then(|e| e.manager.borrow().clone());
        let context = self.context.borrow().clone();
        editing.or_else(|| context.and_then(|c| unsafe { c.GetDocumentMgr().ok() }))
    }
}
impl ITfCompositionSink_Impl for Tip_Impl {
    fn OnCompositionTerminated(&self, _: u32, composition: Ref<ITfComposition>) -> Result<()> {
        self.shared.composition_terminated(composition, None);
        Ok(())
    }
}
impl ITfDisplayAttributeProvider_Impl for Tip_Impl {
    fn EnumDisplayAttributeInfo(&self) -> Result<IEnumTfDisplayAttributeInfo> {
        Ok(attributes::Enumerator::new().into())
    }
    fn GetDisplayAttributeInfo(&self, guid: *const GUID) -> Result<ITfDisplayAttributeInfo> {
        if guid.is_null() || unsafe { *guid } != attributes::ATTRIBUTE {
            return Err(E_INVALIDARG.into());
        }
        Ok(attributes::Attribute::new().into())
    }
}
#[implement(ITfCompositionSink)]
struct CompositionObserver {
    shared: Weak<Shared>,
    generation: u64,
    _guard: ModuleGuard,
}
impl ITfCompositionSink_Impl for CompositionObserver_Impl {
    fn OnCompositionTerminated(&self, _: u32, composition: Ref<ITfComposition>) -> Result<()> {
        if let Some(shared) = self.shared.upgrade() {
            shared.composition_terminated(composition, Some(self.generation));
        }
        Ok(())
    }
}
fn is_enter(key: &Key) -> bool {
    key.special == SpecialKey::Enter as i32 && key.modifiers == 0 && key.text.is_empty()
}

struct NativeDocument<'a> {
    accept_displayed: bool,
    editing: &'a Editing,
    shared: Rc<Shared>,
}
impl Editing {
    fn attached(&self) -> bool {
        let Some(document) = self.manager.borrow().clone() else {
            return false;
        };
        // A pushed context can cover ours. Finish the original mark if its
        // context is still on that stack, even though it no longer has focus.
        unsafe {
            if document.GetTop().as_ref() == Ok(&self.context) {
                return true;
            }
            let Ok(contexts) = document.EnumContexts() else {
                return false;
            };
            let mut slot = [None];
            let mut fetched = 0;
            while contexts.Next(&mut slot, &mut fetched).is_ok() && fetched == 1 {
                if slot[0].take().as_ref() == Some(&self.context) {
                    return true;
                }
            }
            false
        }
    }
}

impl NativeDocument<'_> {
    fn end_owned(&self, reading: Option<&str>) -> bool {
        if self.editing.terminated.get() {
            return true;
        }
        let Some(composition) = self.shared.composition.borrow_mut().take() else {
            return true;
        };
        let finish = reading.map(|r| (ipc::surface(&self.shared.preedit.borrow()), r.to_owned()));
        EndEdit::request(
            &self.editing.context,
            self.shared.tid.get(),
            composition,
            finish,
        )
    }
}
impl Document for NativeDocument<'_> {
    fn apply(&mut self, reply: &KeyReply) -> bool {
        if self.editing.terminated.get()
            || (self.editing.leaving.get()
                && (self.shared.departing.borrow().is_none() || !self.editing.attached()))
            || self.shared.field.get() == FieldScope::Bypass
        {
            return false;
        }
        // TSF can call TestKeyDown from StartComposition, before apply returns.
        self.shared.dispatching_starter.take();
        self.editing
            .policy
            .set(reply.key_policy.unwrap_or_default());
        if reply.commit.is_none()
            && reply.preedit.as_ref().is_none_or(|p| p.segments.is_empty())
            && self
                .shared
                .composition
                .try_borrow()
                .is_ok_and(|c| c.is_none())
        {
            if let Some(page) = &reply.candidates {
                self.shared.show(page);
            } else {
                self.shared.hide();
            }
            return true;
        }
        let edit: ITfEditSession = ApplyEdit {
            shared: self.shared.clone(),
            context: self.editing.context.clone(),
            reply: reply.clone(),
            _guard: ModuleGuard::new(),
        }
        .into();
        let success = unsafe {
            self.editing.context.RequestEditSession(
                self.shared.tid.get(),
                &edit,
                TF_ES_SYNC | TF_ES_READWRITE,
            )
        }
        .is_ok_and(|hr| hr == S_OK);
        if self.editing.terminated.get() {
            return success;
        }
        if self.editing.leaving.get() && success {
            // A queued click on the departing field still needs the acknowledged page.
            self.shared
                .page
                .replace(reply.candidates.clone().unwrap_or_default());
        } else if success {
            if let Some(page) = &reply.candidates {
                self.shared.show(page);
            } else {
                self.shared.hide();
            }
        } else {
            self.end_owned(None);
            *self.shared.preedit.borrow_mut() = Preedit::default();
            self.shared.hide();
        }
        success
    }
    fn finish_reading(&mut self, reading: &str) -> bool {
        let writable = self.shared.field.get() != FieldScope::Bypass;
        let surface;
        let text = if self.accept_displayed {
            surface = ipc::surface(&self.shared.preedit.borrow());
            &surface
        } else {
            reading
        };
        self.end_owned(writable.then_some(text)) && writable
    }
}
#[implement(ITfEditSession)]
struct EndEdit {
    composition: ITfComposition,
    finish: Option<(String, String)>,
    _guard: ModuleGuard,
}
impl EndEdit {
    fn request(
        context: &ITfContext,
        tid: u32,
        composition: ITfComposition,
        finish: Option<(String, String)>,
    ) -> bool {
        let end = |mode, finish| {
            let edit: ITfEditSession = Self {
                composition: composition.clone(),
                finish,
                _guard: ModuleGuard::new(),
            }
            .into();
            unsafe { context.RequestEditSession(tid, &edit, mode | TF_ES_READWRITE) }
        };
        let success = end(TF_ES_SYNC, finish).is_ok_and(|hr| hr == S_OK);
        if !success {
            // Only end the original mark on a deferred grant. A later key may
            // already own another mark, and the host may have changed the text.
            let _ = end(TF_ES_ASYNC, None);
        }
        success
    }
}
impl ITfEditSession_Impl for EndEdit_Impl {
    fn DoEditSession(&self, cookie: u32) -> Result<()> {
        if let Some((surface, reading)) = &self.finish {
            let range = unsafe { self.composition.GetRange()? };
            if range_matches(&range, cookie, surface) {
                if surface != reading {
                    unsafe {
                        range.SetText(cookie, 0, &reading.encode_utf16().collect::<Vec<_>>())?
                    };
                }
                return end_at_caret(&self.composition, &range, cookie);
            }
        }
        unsafe { self.composition.EndComposition(cookie) }
    }
}
fn range_matches(range: &ITfRange, cookie: u32, surface: &str) -> bool {
    // An extra unit distinguishes an exact match from a matching prefix.
    let expected: Vec<_> = surface.encode_utf16().collect();
    let mut actual = vec![0; expected.len() + 1];
    let mut count = 0;
    unsafe { range.GetText(cookie, 0, &mut actual, &mut count) }.is_ok()
        && actual.get(..count as usize) == Some(expected.as_slice())
}
#[implement(ITfEditSession)]
struct ApplyEdit {
    shared: Rc<Shared>,
    context: ITfContext,
    reply: KeyReply,
    _guard: ModuleGuard,
}
impl ITfEditSession_Impl for ApplyEdit_Impl {
    fn DoEditSession(&self, cookie: u32) -> Result<()> {
        if self.shared.departing.borrow().is_some() {
            let mark = self.shared.composition.borrow().clone();
            let surface = ipc::surface(&self.shared.preedit.borrow());
            if let Some(mark) = mark
                && !range_matches(&unsafe { mark.GetRange()? }, cookie, &surface)
            {
                return Err(TF_E_COMPOSITION_REJECTED.into());
            }
        }
        if let Some(commit) = &self.reply.commit {
            write_composition(&self.shared, &self.context, cookie, &commit.value, true)?;
        }
        let preedit = self.reply.preedit.clone().unwrap_or_default();
        let text = ipc::surface(&preedit);
        if !text.is_empty() {
            let range = write_composition(&self.shared, &self.context, cookie, &text, false)?;
            if self.shared.composition_was_terminated() {
                return Ok(());
            }
            // SetText succeeded. Layout and selection are best effort.
            let _ = set_preedit_caret(&self.context, &range, cookie, preedit.cursor_utf16);
            if let Ok(view) = unsafe { self.context.GetActiveView() } {
                let mut rect = RECT::default();
                let mut clipped = BOOL::default();
                if unsafe { view.GetTextExt(cookie, &range, &mut rect, &mut clipped) }.is_ok() {
                    self.shared.caret.set(rect);
                }
                if let Ok(owner) = unsafe { view.GetWnd() } {
                    self.shared.owner.set(owner);
                }
            }
        } else if self.reply.commit.is_none() {
            let composition = self.shared.composition.borrow().clone();
            if let Some(composition) = composition {
                unsafe {
                    let range = composition.GetRange()?;
                    range.SetText(cookie, 0, &[])?;
                    if !self.shared.composition_was_terminated() {
                        composition.EndComposition(cookie)?;
                    }
                }
                self.shared.composition.borrow_mut().take();
            }
        }
        if let Ok(mut current) = self.shared.preedit.try_borrow_mut() {
            *current = preedit;
        }
        Ok(())
    }
}
fn set_preedit_caret(
    context: &ITfContext,
    range: &ITfRange,
    cookie: u32,
    cursor: u32,
) -> Result<()> {
    unsafe {
        let caret = range.Clone()?;
        caret.Collapse(cookie, TF_ANCHOR_START)?;
        let mut shifted = 0;
        caret.ShiftStart(cookie, cursor as i32, &mut shifted, std::ptr::null())?;
        caret.Collapse(cookie, TF_ANCHOR_START)?;
        set_caret(context, cookie, caret)
    }
}

fn end_at_caret(composition: &ITfComposition, range: &ITfRange, cookie: u32) -> Result<()> {
    unsafe {
        // SetText can move the selection back to the start. Keep the endpoint
        // across EndComposition even when the host refuses to move the caret.
        let caret = range.Clone().and_then(|r| {
            r.Collapse(cookie, TF_ANCHOR_END)?;
            Ok(r)
        });
        composition.EndComposition(cookie)?;
        if let Ok(caret) = caret
            && let Ok(context) = range.GetContext()
        {
            let _ = set_caret(&context, cookie, caret);
        }
        Ok(())
    }
}

fn set_caret(context: &ITfContext, cookie: u32, caret: ITfRange) -> Result<()> {
    let mut selection = TF_SELECTION {
        range: std::mem::ManuallyDrop::new(Some(caret)),
        style: TF_SELECTIONSTYLE {
            ase: TF_AE_NONE,
            fInterimChar: false.into(),
        },
    };
    unsafe {
        let result = context.SetSelection(cookie, std::slice::from_ref(&selection));
        std::mem::ManuallyDrop::drop(&mut selection.range);
        result
    }
}
fn write_composition(
    shared: &Rc<Shared>,
    context: &ITfContext,
    cookie: u32,
    text: &str,
    commit: bool,
) -> Result<ITfRange> {
    if shared.composition_was_terminated() {
        return Err(TF_E_COMPOSITION_REJECTED.into());
    }
    let existing = shared.composition.try_borrow().ok().and_then(|c| c.clone());
    let composition = if let Some(c) = existing {
        c
    } else {
        let insert = context.cast::<ITfInsertAtSelection>()?;
        let range = unsafe { insert.InsertTextAtSelection(cookie, TF_IAS_QUERYONLY, &[])? };
        let owner = context.cast::<ITfContextComposition>()?;
        let generation = shared.composition_generation.get().wrapping_add(1);
        shared.composition_generation.set(generation);
        let observer: ITfCompositionSink = CompositionObserver {
            shared: Rc::downgrade(shared),
            generation,
            _guard: ModuleGuard::new(),
        }
        .into();
        // A veto returns S_OK with a null interface. windows-rs represents
        // that as an Error with a success code, which cannot cross a COM sink.
        shared.starting_composition.set(true);
        let started = unsafe { owner.StartComposition(cookie, &range, &observer) };
        shared.starting_composition.set(false);
        let c = started.map_err(|e| {
            if e.code().is_ok() {
                TF_E_COMPOSITION_REJECTED.into()
            } else {
                e
            }
        })?;
        if shared.composition_was_terminated() {
            return Err(TF_E_COMPOSITION_REJECTED.into());
        }
        if let Ok(mut slot) = shared.composition.try_borrow_mut() {
            *slot = Some(c.clone());
        }
        c
    };
    let range = unsafe { composition.GetRange()? };
    let utf16: Vec<_> = text.encode_utf16().collect();
    unsafe {
        range.SetText(cookie, 0, &utf16)?;
    }
    if shared.composition_was_terminated() {
        return Ok(range);
    }
    if !commit
        && shared.attribute.get() != 0
        && let Ok(property) = unsafe { context.GetProperty(&GUID_PROP_ATTRIBUTE) }
    {
        let value = windows::Win32::System::Variant::VARIANT::from(shared.attribute.get() as i32);
        let _ = unsafe { property.SetValue(cookie, &range, &value) };
    }
    if commit {
        end_at_caret(&composition, &range, cookie)?;
        shared.composition.borrow_mut().take();
    }
    Ok(range)
}
#[implement(ITfEditSession)]
struct ScopeEdit {
    shared: Option<Rc<Shared>>,
    field: Cell<FieldScope>,
    context: ITfContext,
    limit: FieldScope,
    read_text: bool,
    _guard: ModuleGuard,
}
impl ITfEditSession_Impl for ScopeEdit_Impl {
    fn DoEditSession(&self, cookie: u32) -> Result<()> {
        let mut selections = [TF_SELECTION::default()];
        let mut count = 0;
        unsafe {
            self.context
                .GetSelection(cookie, TF_DEFAULT_SELECTION, &mut selections, &mut count)?;
        }
        let range = selections
            .first_mut()
            .and_then(|s| unsafe { std::mem::ManuallyDrop::take(&mut s.range) })
            .ok_or_else(failure)?;
        if count != 1 {
            return Err(failure());
        }
        let field = classify_field(&self.context, cookie, &range)?.min(self.limit);
        self.field.set(field);
        let Some(shared) = self.shared.as_ref() else {
            return Ok(());
        };
        if field == FieldScope::Bypass {
            return Ok(());
        }
        let mut app_buffer = [0u16; 32768];
        let len = unsafe { GetModuleFileNameW(None, &mut app_buffer) } as usize;
        let app = app_buffer
            .get(..len)
            .and_then(|b| String::from_utf16(b).ok())
            .unwrap_or_default();
        let privacy = if field == FieldScope::Private {
            Privacy::Private
        } else if let Some(editing) = shared.editing.borrow().as_ref() {
            editing.app_privacy
        } else {
            shared
                .client
                .try_borrow()
                .ok()
                .and_then(|c| c.as_ref().map(|c| c.privacy_for_app(&app)))
                .unwrap_or(Privacy::Unknown)
        };
        let mut observed = ipc::Context {
            privacy: privacy as i32,
            app,
            ..Default::default()
        };
        if self.read_text && privacy == Privacy::Normal {
            let before = if let Some(composition) =
                shared.composition.try_borrow().ok().and_then(|c| c.clone())
            {
                unsafe { composition.GetRange()? }
            } else {
                range
            };
            unsafe {
                before.Collapse(cookie, TF_ANCHOR_START)?;
            }
            let mut moved = 0;
            unsafe {
                before.ShiftStart(cookie, -512, &mut moved, std::ptr::null())?;
            }
            let mut buffer = [0u16; 512];
            let mut len = 0;
            if unsafe { before.GetText(cookie, 0, &mut buffer, &mut len) }.is_ok()
                && let Some(text) = buffer
                    .get(..len as usize)
                    .and_then(|b| String::from_utf16(b).ok())
                    .filter(|s| s.len() <= ipc::MAX_CONTEXT)
            {
                observed.preceding_text = text;
                observed.surrounding_available = true;
            }
        }
        *shared.observed.try_borrow_mut().map_err(|_| failure())? = observed;
        Ok(())
    }
}
// A later check for the same key can only make its field policy stricter.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FieldScope {
    Bypass,
    Private,
    Ordinary,
}

fn keyboard_enabled(context: &ITfContext) -> bool {
    use windows::Win32::System::Variant::VT_I4;
    let value = context.cast::<ITfCompartmentMgr>().and_then(|mgr| unsafe {
        mgr.GetCompartment(&GUID_COMPARTMENT_KEYBOARD_DISABLED)?
            .GetValue()
    });
    value.is_ok_and(|value| unsafe {
        let inner = &value.Anonymous.Anonymous;
        inner.vt == VT_EMPTY || (inner.vt == VT_I4 && inner.Anonymous.lVal == 0)
    })
}

fn range_attribute(
    context: &ITfContext,
    cookie: u32,
    range: &ITfRange,
    id: &GUID,
) -> Result<Option<VARIANT>> {
    unsafe {
        let Ok(prop) = context.GetAppProperty(id) else {
            return Ok(None);
        };
        if !range.IsEmpty(cookie)?.as_bool() {
            // GetValue can describe only the start of a mixed selection. Stores
            // without FindNextAttrTransition can still supply a usable value.
            let mut ranges = None;
            if prop.EnumRanges(cookie, &mut ranges, range).is_ok()
                && let Some(ranges) = ranges
            {
                let mut spans = [None, None];
                let mut count = 0;
                if ranges.Next(&mut spans, &mut count).is_ok() && count == 2 {
                    return Err(failure());
                }
            }
        }
        Ok(prop
            .GetValue(cookie, range)
            .ok()
            .filter(|value| value.Anonymous.Anonymous.vt != VT_EMPTY))
    }
}

fn classify_field(context: &ITfContext, cookie: u32, range: &ITfRange) -> Result<FieldScope> {
    if unsafe { context.GetStatus()? }.dwDynamicFlags & TS_SD_READONLY != 0
        || !keyboard_enabled(context)
    {
        return Ok(FieldScope::Bypass);
    }
    if let Some(value) = range_attribute(context, cookie, range, &TSATTRID_Text_ReadOnly)? {
        let inner = unsafe { &value.Anonymous.Anonymous };
        if inner.vt == VT_BOOL && unsafe { inner.Anonymous.boolVal } != VARIANT_FALSE {
            return Ok(FieldScope::Bypass);
        }
    }
    let Some(value) = range_attribute(context, cookie, range, &GUID_PROP_INPUTSCOPE)? else {
        return Ok(FieldScope::Ordinary);
    };
    let scope: ITfInputScope = IUnknown::try_from(&value)?.cast()?;
    unsafe {
        let mut scopes = std::ptr::null_mut();
        let mut len = 0;
        scope.GetInputScopes(&mut scopes, &mut len)?;
        let field = if len == 0 {
            FieldScope::Ordinary
        } else if scopes.is_null() {
            FieldScope::Bypass
        } else {
            let scopes = std::slice::from_raw_parts(scopes, len.min(256) as usize);
            if scopes.iter().any(|s| {
                matches!(
                    *s,
                    IS_PASSWORD
                        | IS_NUMERIC_PASSWORD
                        | IS_NUMERIC_PIN
                        | IS_ALPHANUMERIC_PIN
                        | IS_ALPHANUMERIC_PIN_SET
                )
            }) {
                FieldScope::Bypass
            } else if scopes.contains(&IS_PRIVATE) {
                FieldScope::Private
            } else {
                FieldScope::Ordinary
            }
        };
        CoTaskMemFree(Some(scopes.cast()));
        Ok(field)
    }
}

fn refresh_scope(
    shared: &Rc<Shared>,
    context: &ITfContext,
    limit: FieldScope,
    read_text: bool,
) -> FieldScope {
    try_refresh_scope(shared, context, limit, read_text).unwrap_or(FieldScope::Bypass)
}

fn try_refresh_scope(
    shared: &Rc<Shared>,
    context: &ITfContext,
    limit: FieldScope,
    read_text: bool,
) -> Option<FieldScope> {
    let field = query_scope(Some(shared), shared.tid.get(), context, limit, read_text);
    shared.field.set(field.unwrap_or(FieldScope::Bypass));
    field
}

fn query_scope(
    shared: Option<&Rc<Shared>>,
    tid: u32,
    context: &ITfContext,
    limit: FieldScope,
    read_text: bool,
) -> Option<FieldScope> {
    let edit = windows_core::ComObject::new(ScopeEdit {
        shared: shared.cloned(),
        field: Cell::new(FieldScope::Bypass),
        context: context.clone(),
        limit,
        read_text,
        _guard: ModuleGuard::new(),
    });
    let result = unsafe {
        context.RequestEditSession(
            tid,
            &edit.to_interface::<ITfEditSession>(),
            TF_ES_SYNC | TF_ES_READ,
        )
    };
    result
        .is_ok_and(|status| status == S_OK)
        .then(|| edit.field.get())
}
fn starter_virtual_key(vk: u32) -> bool {
    (0x30..=0x5a).contains(&vk) || (0xba..=0xe2).contains(&vk)
}
fn is_starter(w: WPARAM, key: &Key) -> bool {
    starter_virtual_key(w.0 as u32) && ipc::frontend::starts_composition(key)
}
fn translate(w: WPARAM, l: LPARAM) -> Option<Key> {
    let vk = w.0 as u32;
    let special = match vk {
        0x20 => SpecialKey::Space,
        0x0d => SpecialKey::Enter,
        8 => SpecialKey::Backspace,
        0x1b => SpecialKey::Escape,
        0x25 => SpecialKey::Left,
        0x27 => SpecialKey::Right,
        0x26 => SpecialKey::Up,
        0x28 => SpecialKey::Down,
        9 => SpecialKey::Tab,
        0x2e => SpecialKey::Delete,
        0x24 => SpecialKey::Home,
        0x23 => SpecialKey::End,
        _ => SpecialKey::None,
    };
    let mut modifiers = 0;
    for (key, mask) in [
        (VK_SHIFT, ipc::MOD_SHIFT),
        (VK_CONTROL, ipc::MOD_CONTROL),
        (VK_MENU, ipc::MOD_ALT),
        (VK_LWIN, ipc::MOD_META),
        (VK_RWIN, ipc::MOD_META),
    ] {
        if unsafe { GetKeyState(key.0 as i32) } < 0 {
            modifiers |= mask;
        }
    }
    if (0x75..=0x79).contains(&vk) {
        return ipc::frontend::function_key(vk - 0x6f, modifiers);
    }
    let text = if special != SpecialKey::None {
        String::new()
    } else if modifiers == ipc::MOD_CONTROL && matches!(vk, 0x55 | 0x49 | 0x4f | 0x50 | 0x54) {
        char::from_u32(vk)?.to_ascii_lowercase().to_string()
    } else {
        let mut keys = [0u8; 256];
        if unsafe { GetKeyboardState(&mut keys) }.is_err() {
            return None;
        }
        let mut chars = [0u16; 8];
        let len = unsafe {
            ToUnicodeEx(
                vk,
                ((l.0 >> 16) & 0xff) as u32,
                &keys,
                &mut chars,
                4,
                Some(GetKeyboardLayout(0)),
            )
        };
        if len <= 0 {
            return None;
        }
        String::from_utf16(chars.get(..len as usize)?).ok()?
    };
    Some(Key {
        text,
        special: special as i32,
        modifiers,
    })
}
#[implement(IClassFactory)]
struct Factory {
    _guard: ModuleGuard,
}
impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<IUnknown>,
        iid: *const GUID,
        out: *mut *mut c_void,
    ) -> Result<()> {
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        if iid.is_null() || out.is_null() {
            return Err(E_POINTER.into());
        }
        let object: ITfTextInputProcessorEx = Tip::new().into();
        unsafe { object.query(iid, out).ok() }
    }
    fn LockServer(&self, lock: BOOL) -> Result<()> {
        if lock.as_bool() {
            LOCKS.fetch_add(1, Ordering::AcqRel);
        } else {
            let _ = LOCKS.try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
        }
        Ok(())
    }
}
#[unsafe(no_mangle)]
pub extern "system" fn DllMain(module: HMODULE, reason: u32, _: *mut c_void) -> BOOL {
    if reason == 1 {
        MODULE.store(module.0 as usize, Ordering::Release);
    }
    true.into()
}
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    class: *const GUID,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if class.is_null() || iid.is_null() || out.is_null() {
        return E_POINTER;
    }
    unsafe {
        *out = std::ptr::null_mut();
        if *class != CLSID {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
    }
    let factory: IClassFactory = Factory {
        _guard: ModuleGuard::new(),
    }
    .into();
    unsafe { factory.query(iid, out) }
}
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if OBJECTS.load(Ordering::Acquire) == 0 && LOCKS.load(Ordering::Acquire) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}
#[unsafe(no_mangle)]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    registration::register().into()
}
#[unsafe(no_mangle)]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    registration::unregister().into()
}
