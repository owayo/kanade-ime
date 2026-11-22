#![allow(non_snake_case)]
use kanade_ipc::{
    self as ipc, Key, KeyReply, Preedit, Privacy, SpecialKey,
    frontend::{Client, Document},
    windows::Pipe,
};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
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
    eaten_keys: RefCell<BTreeSet<usize>>,
    client: RefCell<Option<Client<SessionPipe>>>,
    composition: RefCell<Option<ITfComposition>>,
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
            eaten_keys: RefCell::new(BTreeSet::new()),
            client: RefCell::new(None),
            composition: RefCell::new(None),
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
    fn put_client(&self, client: Client<SessionPipe>) {
        if !client.edit_rejected()
            && let Ok(mut slot) = self.client.try_borrow_mut()
        {
            *slot = Some(client);
        }
    }
    fn composition_terminated(&self, composition: Ref<ITfComposition>) {
        let Some(ended) = composition.as_ref() else {
            return;
        };
        if self.composition.borrow().as_ref() != Some(ended) {
            return;
        }
        self.composition.borrow_mut().take();
        // The host already decided what stays in the document.
        self.client.borrow_mut().take();
        *self.preedit.borrow_mut() = Preedit::default();
        *self.page.borrow_mut() = ipc::CandidatePage::default();
        self.hide();
    }
    fn outage_scope(&self) -> bool {
        self.field.get() != FieldScope::Bypass && !self.secure.get()
    }
    /// Only an idle, disconnected session can drop a starter.
    fn can_start_offline(&self) -> bool {
        self.client
            .try_borrow()
            .is_ok_and(|slot| slot.as_ref().is_none_or(Client::can_start_offline))
    }
    fn hide(&self) {
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
        if let Ok(mut hint) = self.mode_hint.try_borrow_mut() {
            hint.take();
        }
        if let Ok(mut keys) = self.eaten_keys.try_borrow_mut() {
            keys.clear();
        }
        self.hide();
        let client = self.client.try_borrow_mut().ok().and_then(|mut c| c.take());
        let context = self.context.try_borrow().ok().and_then(|c| c.clone());
        if let (Some(mut client), Some(context)) = (client, context) {
            client.finish(&mut NativeDocument {
                shared: self.clone(),
                context,
            });
        }
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
        if let Ok(mut page) = self.page.try_borrow_mut() {
            *page = ipc::CandidatePage::default();
        }
    }
    fn suspend_secure(&self) {
        self.tested.set(None);
        if let Ok(mut keys) = self.eaten_keys.try_borrow_mut() {
            keys.clear();
        }
        self.hide();
        if let Ok(mut client) = self.client.try_borrow_mut() {
            client.take();
        }
        if let Ok(mut composition) = self.composition.try_borrow_mut() {
            composition.take();
        }
        if let Ok(mut context) = self.context.try_borrow_mut() {
            context.take();
        }
        if let Ok(mut preedit) = self.preedit.try_borrow_mut() {
            *preedit = Preedit::default();
        }
        if let Ok(mut observed) = self.observed.try_borrow_mut() {
            *observed = ipc::Context::default();
        }
        if let Ok(mut page) = self.page.try_borrow_mut() {
            *page = ipc::CandidatePage::default();
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
        if refresh_scope(self, &context, FieldScope::Ordinary, false) == FieldScope::Bypass {
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
        let client = self.client.try_borrow_mut().ok().and_then(|mut c| c.take());
        let Some(mut client) = client else {
            if expired {
                page.mark_refinement_timeout();
                self.show(&page);
            }
            return;
        };
        let update = client.poll_with_preview(
            &page,
            &mut NativeDocument {
                shared: self.clone(),
                context,
            },
        );
        self.put_client(client);
        match update {
            Ok(Some(page)) => self.show(&page),
            Ok(None) if expired => {
                page.mark_refinement_timeout();
                self.show(&page);
            }
            Ok(None) => {}
            Err(_) => self.hide(),
        }
    }
    fn choose(self: &Rc<Self>, handle: ipc::CandidateHandle) {
        self.stop_refinement();
        let context = self.context.try_borrow().ok().and_then(|c| c.clone());
        let Some(context) = context else {
            self.suspend_secure();
            return;
        };
        if refresh_scope(self, &context, FieldScope::Ordinary, false) == FieldScope::Bypass {
            self.suspend_secure();
            return;
        }
        let context = self.context.try_borrow().ok().and_then(|c| c.clone());
        let client = self.client.try_borrow_mut().ok().and_then(|mut c| c.take());
        if let (Some(context), Some(mut client)) = (context, client) {
            client.select(
                handle,
                &mut NativeDocument {
                    shared: self.clone(),
                    context,
                },
            );
            self.put_client(client);
        }
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
    sink_cookie: Cell<Option<u32>>,
    mode: RefCell<Option<input_mode::Mode>>,
    button: RefCell<Option<language_bar::Item>>,
    japanese: Cell<bool>,
    show_mode_hint: Cell<bool>,
    _guard: ModuleGuard,
}
impl Tip {
    fn new() -> Self {
        Self {
            shared: Shared::new(),
            sink_cookie: Cell::new(None),
            mode: RefCell::new(None),
            button: RefCell::new(None),
            japanese: Cell::new(false),
            show_mode_hint: Cell::new(true),
            _guard: ModuleGuard::new(),
        }
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
        Ok(())
    }
}
impl ITfTextInputProcessorEx_Impl for Tip_Impl {
    fn ActivateEx(&self, manager: Ref<ITfThreadMgr>, tid: u32, flags: u32) -> Result<()> {
        let manager = manager.ok()?;
        self.shared.secure.set(flags & TF_TMAE_SECUREMODE != 0);
        self.show_mode_hint
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
            })?;
        self.sink_cookie.set(Some(cookie));
        self.japanese.set(mode.japanese());
        *self.mode.borrow_mut() = Some(mode);
        if let Ok(mut slot) = self.button.try_borrow_mut() {
            *slot = button;
        }
        if self.japanese.get() {
            self.shared.connect(false);
        }
        Ok(())
    }
}
impl ITfCompartmentEventSink_Impl for Tip_Impl {
    fn OnChange(&self, _: *const GUID) -> Result<()> {
        if let Some(mode) = self.mode.borrow().as_ref() {
            let japanese = mode.japanese();
            if self.japanese.replace(japanese) && !japanese {
                self.shared.reset();
            }
        }
        self.sync_button();
        Ok(())
    }
}
impl ITfKeyEventSink_Impl for Tip_Impl {
    fn OnSetFocus(&self, foreground: BOOL) -> Result<()> {
        if foreground.as_bool() {
            // The taskbar theme can change while another thread has the focus.
            self.sync_button();
        } else {
            self.shared.reset();
        }
        Ok(())
    }
    fn OnTestKeyDown(&self, context: Ref<ITfContext>, w: WPARAM, l: LPARAM) -> Result<BOOL> {
        let previous = self.shared.tested.take();
        self.refresh_button();
        if input_mode::physical(w.0).is_some() {
            return Ok(true.into());
        }
        if !self.japanese.get() {
            return Ok(false.into());
        }
        let context = context.ok()?;
        let message = KeyMessage::current(context, w, l);
        if let Some(tested) = previous.filter(|prior| prior.message == message) {
            self.shared.tested.set(Some(tested));
            return Ok((tested.decision != KeyTest::Passed).into());
        }
        let field = refresh_scope(&self.shared, context, FieldScope::Ordinary, false);
        if field == FieldScope::Bypass {
            self.shared.suspend_secure();
            self.shared.tested.set(Some(TestedKey {
                message,
                decision: KeyTest::Passed,
                field,
            }));
            return Ok(false.into());
        }
        let connection = self.shared.connect(false);
        let key = translate(w, l, true);
        let engine_owns = key.as_ref().is_some_and(|k| {
            self.shared.client.try_borrow().is_ok_and(|c| {
                c.as_ref()
                    .is_some_and(|c| c.would_consume(k, Privacy::Unknown))
            })
        });
        let actual = translate(w, l, false);
        let outage_starter =
            self.shared.outage_scope() && actual.as_ref().is_some_and(|k| is_starter(w, k));
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
            if let Some(key) = actual
                && !self.shared.can_start_offline()
            {
                if refresh_scope(&self.shared, context, field, true) == FieldScope::Bypass {
                    self.shared.suspend_secure();
                } else {
                    self.send_key(context, key, true, false);
                }
            }
            KeyTest::Passed
        };
        self.shared.tested.set(Some(TestedKey {
            message,
            decision: test,
            field,
        }));
        Ok((test != KeyTest::Passed).into())
    }
    fn OnTestKeyUp(&self, _: Ref<ITfContext>, w: WPARAM, _: LPARAM) -> Result<BOOL> {
        self.shared.tested.set(None);
        Ok(self
            .shared
            .eaten_keys
            .try_borrow()
            .is_ok_and(|keys| keys.contains(&w.0))
            .into())
    }
    fn OnKeyDown(&self, context: Ref<ITfContext>, w: WPARAM, l: LPARAM) -> Result<BOOL> {
        let previous = self.shared.tested.take();
        self.refresh_button();
        if let Some(command) = input_mode::physical(w.0) {
            if l.0 & (1 << 30) == 0 {
                self.set_input_mode(command, context.as_ref())?;
            }
            self.shared.eaten_keys.borrow_mut().insert(w.0);
            return Ok(true.into());
        }
        if !self.japanese.get() {
            return Ok(false.into());
        }
        let context = context.ok()?;
        let message = KeyMessage::current(context, w, l);
        let test = previous.filter(|prior| prior.message == message);
        if test.is_some_and(|key| key.decision == KeyTest::Passed) {
            return Ok(false.into());
        }
        let consumed = self.process_key(context, w, l, test);
        if w.0 <= 255
            && let Ok(mut keys) = self.shared.eaten_keys.try_borrow_mut()
        {
            if consumed {
                keys.insert(w.0);
            } else {
                keys.remove(&w.0);
            }
        }
        Ok(consumed.into())
    }
    fn OnKeyUp(&self, _: Ref<ITfContext>, w: WPARAM, _: LPARAM) -> Result<BOOL> {
        self.shared.tested.set(None);
        Ok(self
            .shared
            .eaten_keys
            .try_borrow_mut()
            .is_ok_and(|mut keys| keys.remove(&w.0))
            .into())
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
    fn set_input_mode(
        &self,
        command: input_mode::Command,
        context: Option<&ITfContext>,
    ) -> Result<()> {
        if let Some(mode) = self.mode.borrow().as_ref() {
            mode.apply(self.shared.tid.get(), command)?;
        }
        self.OnChange(std::ptr::null())?;
        if self.show_mode_hint.get()
            && let Some(context) = context
        {
            let hint =
                mode_hint::Hint::at_selection(context, self.shared.tid.get(), self.japanese.get());
            if let Ok(mut previous) = self.shared.mode_hint.try_borrow_mut() {
                *previous = hint.ok();
            }
        }
        Ok(())
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
        let field = refresh_scope(
            &self.shared,
            context,
            tested.map_or(FieldScope::Ordinary, |key| key.field),
            true,
        );
        if field == FieldScope::Bypass {
            self.shared.suspend_secure();
            return test.is_some();
        }
        if offline_claim && !self.shared.outage_scope() {
            return true;
        }
        let Some(key) = translate(w, l, false) else {
            return claimed;
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
            self.report_outage(context);
            return true;
        }
        let connection = self.shared.connect(starter && self.shared.outage_scope());
        if connection == Connection::Attached {
            // The welcome supplied the app learning policy for this first key.
            if refresh_scope(&self.shared, context, field, true) == FieldScope::Bypass {
                self.shared.suspend_secure();
                return test.is_some();
            }
            if offline_claim && !self.shared.outage_scope() {
                return true;
            }
        }
        let eligible = starter && self.shared.outage_scope();
        if self.shared.can_start_offline() {
            if eligible {
                self.report_outage(context);
            }
            return claimed || eligible;
        }
        let consumed = self
            .send_key(context, key, false, eligible)
            .unwrap_or(claimed);
        consumed || offline_claim || (claimed && connection == Connection::Attached)
    }
    fn send_key(
        &self,
        context: &ITfContext,
        key: Key,
        passed: bool,
        eligible: bool,
    ) -> Option<bool> {
        if let Ok(mut saved) = self.shared.context.try_borrow_mut() {
            *saved = Some(context.clone());
        }
        let observed = self.shared.observed.try_borrow().ok()?.clone();
        let mut client = self
            .shared
            .client
            .try_borrow_mut()
            .ok()
            .and_then(|mut c| c.take())?;
        self.shared.stop_refinement();
        let connected = client.is_connected();
        let drop_on_loss = eligible && client.would_consume(&key, Privacy::Unknown);
        let result = client.key_result(
            key,
            observed,
            passed,
            &mut NativeDocument {
                shared: self.shared.clone(),
                context: context.clone(),
            },
        );
        // Candidates of a lost session cannot be chosen any more.
        let held = client.is_held();
        self.shared.put_client(client);
        if held {
            self.shared.hide();
        }
        if connected && result.is_some() {
            // A handshake alone cannot tell whether the engine can process keys.
            if let Ok(mut hint) = self.shared.mode_hint.try_borrow_mut() {
                hint.take_if(|h| h.outage_shown_at.is_some());
            }
        } else if drop_on_loss && !held && result.is_none() {
            // A missing pipe may need the supervisor. Leave the next Hello
            // for the next key, even if the stalled peer still accepts opens.
            drop((self.shared.open_pipe)(true));
            self.report_outage(context);
            return Some(true);
        }
        result.or(held.then_some(true))
    }
    fn report_outage(&self, context: &ITfContext) {
        const REPEAT_AFTER: Duration = Duration::from_secs(10);
        if !self.show_mode_hint.get()
            || self
                .shared
                .mode_hint
                .try_borrow()
                .ok()
                .and_then(|hint| hint.as_ref()?.outage_shown_at)
                .is_some_and(|shown| shown.elapsed() < REPEAT_AFTER)
        {
            return;
        }
        let hint = mode_hint::Hint::for_outage(context, self.shared.tid.get());
        if let Ok(hint) = hint
            && let Ok(mut previous) = self.shared.mode_hint.try_borrow_mut()
        {
            *previous = Some(hint);
        }
    }
}
impl ITfThreadMgrEventSink_Impl for Tip_Impl {
    fn OnInitDocumentMgr(&self, _: Ref<ITfDocumentMgr>) -> Result<()> {
        Ok(())
    }
    fn OnUninitDocumentMgr(&self, _: Ref<ITfDocumentMgr>) -> Result<()> {
        self.shared.reset();
        Ok(())
    }
    fn OnSetFocus(&self, _: Ref<ITfDocumentMgr>, _: Ref<ITfDocumentMgr>) -> Result<()> {
        self.shared.reset();
        Ok(())
    }
    fn OnPushContext(&self, _: Ref<ITfContext>) -> Result<()> {
        self.shared.reset();
        Ok(())
    }
    fn OnPopContext(&self, _: Ref<ITfContext>) -> Result<()> {
        self.shared.reset();
        Ok(())
    }
}
impl ITfCompositionSink_Impl for Tip_Impl {
    fn OnCompositionTerminated(&self, _: u32, composition: Ref<ITfComposition>) -> Result<()> {
        self.shared.composition_terminated(composition);
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
    _guard: ModuleGuard,
}
impl ITfCompositionSink_Impl for CompositionObserver_Impl {
    fn OnCompositionTerminated(&self, _: u32, composition: Ref<ITfComposition>) -> Result<()> {
        if let Some(shared) = self.shared.upgrade() {
            shared.composition_terminated(composition);
        }
        Ok(())
    }
}
struct NativeDocument {
    shared: Rc<Shared>,
    context: ITfContext,
}
impl NativeDocument {
    fn request_write(
        &self,
        edit: &ITfEditSession,
        mode: TF_CONTEXT_EDIT_CONTEXT_FLAGS,
    ) -> Result<HRESULT> {
        unsafe {
            self.context
                .RequestEditSession(self.shared.tid.get(), edit, mode | TF_ES_READWRITE)
        }
    }

    // Capture the original composition: a deferred end must never touch a
    // composition started by a later key.
    fn end_owned(&self, reading: Option<&str>) -> bool {
        let Some(composition) = self.shared.composition.borrow_mut().take() else {
            return true;
        };
        let finish = reading.map(|r| (ipc::surface(&self.shared.preedit.borrow()), r.to_owned()));
        let end = |finish| -> ITfEditSession {
            EndEdit {
                composition: composition.clone(),
                finish,
                _guard: ModuleGuard::new(),
            }
            .into()
        };
        let success = self
            .request_write(&end(finish), TF_ES_SYNC)
            .is_ok_and(|hr| hr == S_OK);
        if !success {
            // A store may deny synchronous writes while it owns the lock.
            // Only ending the mark can be deferred, never the text edit.
            let _ = self.request_write(&end(None), TF_ES_ASYNC);
        }
        success
    }
}
impl Document for NativeDocument {
    fn apply(&mut self, reply: &KeyReply) -> bool {
        if self.shared.field.get() == FieldScope::Bypass {
            return false;
        }
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
            context: self.context.clone(),
            reply: reply.clone(),
            _guard: ModuleGuard::new(),
        }
        .into();
        let success = self
            .request_write(&edit, TF_ES_SYNC)
            .is_ok_and(|hr| hr == S_OK);
        if success {
            if let Some(page) = &reply.candidates {
                self.shared.show(page);
            } else {
                self.shared.hide();
            }
        } else {
            self.end_owned(None);
            *self.shared.preedit.borrow_mut() = Preedit::default();
            *self.shared.page.borrow_mut() = ipc::CandidatePage::default();
            self.shared.hide();
        }
        success
    }
    fn finish_reading(&mut self, reading: &str) -> bool {
        let writable = self.shared.field.get() != FieldScope::Bypass;
        self.end_owned(writable.then_some(reading)) && writable
    }
}
#[implement(ITfEditSession)]
struct EndEdit {
    composition: ITfComposition,
    finish: Option<(String, String)>,
    _guard: ModuleGuard,
}
impl ITfEditSession_Impl for EndEdit_Impl {
    fn DoEditSession(&self, cookie: u32) -> Result<()> {
        if let Some((surface, reading)) = &self.finish {
            let range = unsafe { self.composition.GetRange()? };
            // An extra unit distinguishes an exact match from a matching prefix.
            let expected: Vec<_> = surface.encode_utf16().collect();
            let mut actual = vec![0; expected.len() + 1];
            let mut count = 0;
            if unsafe { range.GetText(cookie, 0, &mut actual, &mut count) }.is_ok()
                && actual.get(..count as usize) == Some(expected.as_slice())
            {
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
#[implement(ITfEditSession)]
struct ApplyEdit {
    shared: Rc<Shared>,
    context: ITfContext,
    reply: KeyReply,
    _guard: ModuleGuard,
}
impl ITfEditSession_Impl for ApplyEdit_Impl {
    fn DoEditSession(&self, cookie: u32) -> Result<()> {
        if let Some(commit) = &self.reply.commit {
            write_composition(&self.shared, &self.context, cookie, &commit.value, true)?;
        }
        let preedit = self.reply.preedit.clone().unwrap_or_default();
        let text = ipc::surface(&preedit);
        if !text.is_empty() {
            let range = write_composition(&self.shared, &self.context, cookie, &text, false)?;
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
                    composition.EndComposition(cookie)?;
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
    let existing = shared.composition.try_borrow().ok().and_then(|c| c.clone());
    let composition = if let Some(c) = existing {
        c
    } else {
        let insert = context.cast::<ITfInsertAtSelection>()?;
        let range = unsafe { insert.InsertTextAtSelection(cookie, TF_IAS_QUERYONLY, &[])? };
        let owner = context.cast::<ITfContextComposition>()?;
        let observer: ITfCompositionSink = CompositionObserver {
            shared: Rc::downgrade(shared),
            _guard: ModuleGuard::new(),
        }
        .into();
        // A veto returns S_OK with a null interface. windows-rs represents
        // that as an Error with a success code, which cannot cross a COM sink.
        let c = unsafe { owner.StartComposition(cookie, &range, &observer) }.map_err(|e| {
            if e.code().is_ok() {
                TF_E_COMPOSITION_REJECTED.into()
            } else {
                e
            }
        })?;
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
    shared: Rc<Shared>,
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
        } else {
            self.shared
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
            let before = if let Some(composition) = self
                .shared
                .composition
                .try_borrow()
                .ok()
                .and_then(|c| c.clone())
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
        *self
            .shared
            .observed
            .try_borrow_mut()
            .map_err(|_| failure())? = observed;
        self.shared.field.set(field);
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
    shared.field.set(FieldScope::Bypass);
    let edit: ITfEditSession = ScopeEdit {
        shared: shared.clone(),
        context: context.clone(),
        limit,
        read_text,
        _guard: ModuleGuard::new(),
    }
    .into();
    let result =
        unsafe { context.RequestEditSession(shared.tid.get(), &edit, TF_ES_SYNC | TF_ES_READ) };
    if !result.is_ok_and(|status| status == S_OK) {
        shared.field.set(FieldScope::Bypass);
    }
    shared.field.get()
}
fn starter_virtual_key(vk: u32) -> bool {
    (0x30..=0x5a).contains(&vk) || (0xba..=0xe2).contains(&vk)
}
fn is_starter(w: WPARAM, key: &Key) -> bool {
    starter_virtual_key(w.0 as u32) && ipc::frontend::starts_composition(key)
}
fn translate(w: WPARAM, l: LPARAM, test: bool) -> Option<Key> {
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
    } else if test {
        if starter_virtual_key(vk) {
            "x".into()
        } else {
            return None;
        }
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
