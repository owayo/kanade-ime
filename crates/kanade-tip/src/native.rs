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
};
use windows::{
    Win32::{
        Foundation::*,
        System::{Com::*, LibraryLoader::GetModuleFileNameW},
        UI::{Input::KeyboardAndMouse::*, TextServices::*},
    },
    core::{BOOL, Error, GUID, HRESULT, IUnknown, Interface, Ref, Result, implement},
};
use windows_core::IUnknownImpl;
mod attributes;
mod candidates;
mod input_mode;
mod mode_hint;
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
struct Shared {
    observed: RefCell<ipc::Context>,
    attribute: Cell<u32>,
    ui_manager: RefCell<Option<ITfUIElementMgr>>,
    ui_element: RefCell<Option<(u32, ITfUIElement)>>,
    page: RefCell<ipc::CandidatePage>,
    ui_native: Cell<bool>,
    ui_selection: Cell<u32>,
    notified: Cell<Option<(i32, usize, isize, usize)>>,
    eaten_keys: RefCell<BTreeSet<usize>>,
    client: RefCell<Option<Client<Pipe>>>,
    composition: RefCell<Option<ITfComposition>>,
    context: RefCell<Option<ITfContext>>,
    manager: RefCell<Option<ITfThreadMgr>>,
    tid: Cell<u32>,
    password: Cell<bool>,
    preedit: RefCell<Preedit>,
    window: RefCell<Option<CandidateWindow>>,
    mode_hint: RefCell<Option<mode_hint::Hint>>,
    caret: Cell<RECT>,
    owner: Cell<HWND>,
}
impl Shared {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            observed: RefCell::new(ipc::Context::default()),
            attribute: Cell::new(0),
            ui_manager: RefCell::new(None),
            ui_element: RefCell::new(None),
            page: RefCell::new(ipc::CandidatePage::default()),
            ui_native: Cell::new(true),
            ui_selection: Cell::new(0),
            notified: Cell::new(None),
            eaten_keys: RefCell::new(BTreeSet::new()),
            client: RefCell::new(None),
            composition: RefCell::new(None),
            context: RefCell::new(None),
            manager: RefCell::new(None),
            tid: Cell::new(0),
            password: Cell::new(true),
            preedit: RefCell::new(Preedit::default()),
            window: RefCell::new(None),
            mode_hint: RefCell::new(None),
            caret: Cell::new(RECT::default()),
            owner: Cell::new(HWND::default()),
        })
    }
    /// Gives the client a connection when it has none. An existing client is
    /// kept: it may hold a composition that the new session will restore.
    fn connect(&self) {
        if let Ok(mut c) = self.client.try_borrow_mut() {
            if c.as_mut().is_some_and(|c| !c.needs_transport()) {
                return;
            }
            let pipe = module_path().ok().and_then(|p| {
                let folder = p.parent()?;
                // The 32-bit TIP ships in x86/ under the engine's directory.
                let root = if folder.file_name().is_some_and(|name| name == "x86") {
                    folder.parent()?
                } else {
                    folder
                };
                Pipe::connect(&root.join("kanade-engine.exe")).ok()
            });
            match (c.as_mut(), pipe) {
                (Some(client), Some(pipe)) => {
                    let _ = client.reattach(pipe);
                }
                (None, Some(pipe)) => *c = Client::connect(pipe).ok(),
                (_, None) => {}
            }
        }
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
        self.notified.set(None);
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
        self.notified.set(None);
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
        if refresh_scope_with_text(self, &context, false).is_err() || self.password.get() {
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
        if let Ok(mut stored) = self.client.try_borrow_mut() {
            *stored = Some(client);
        }
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
        if refresh_scope(self, &context).is_err() || self.password.get() {
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
            if let Ok(mut c) = self.client.try_borrow_mut() {
                *c = Some(client);
            }
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
            japanese: Cell::new(false),
            show_mode_hint: Cell::new(true),
            _guard: ModuleGuard::new(),
        }
    }
}
impl ITfTextInputProcessor_Impl for Tip_Impl {
    fn Activate(&self, manager: Ref<ITfThreadMgr>, tid: u32) -> Result<()> {
        self.ActivateEx(manager, tid, 0)
    }
    fn Deactivate(&self) -> Result<()> {
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
        if self.japanese.get() {
            self.shared.connect();
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
        Ok(())
    }
}
impl ITfKeyEventSink_Impl for Tip_Impl {
    fn OnSetFocus(&self, foreground: BOOL) -> Result<()> {
        if !foreground.as_bool() {
            self.shared.reset();
        }
        Ok(())
    }
    fn OnTestKeyDown(&self, context: Ref<ITfContext>, w: WPARAM, l: LPARAM) -> Result<BOOL> {
        if input_mode::physical(w.0).is_some() {
            return Ok(true.into());
        }
        if !self.japanese.get() {
            return Ok(false.into());
        }
        let context = context.ok()?;
        let _ = refresh_scope(&self.shared, context);
        if self.shared.password.get() {
            self.shared.suspend_secure();
            return Ok(false.into());
        }
        self.shared.connect();
        let key = translate(w, l, true);
        let consume = key.as_ref().is_some_and(|k| {
            self.shared.client.try_borrow().ok().is_some_and(|c| {
                c.as_ref()
                    .is_some_and(|c| c.would_consume(k, Privacy::Unknown))
            })
        });
        // Unconsumed keys may not reach OnKeyDown. Record them once here.
        let signature = (
            unsafe { windows::Win32::UI::WindowsAndMessaging::GetMessageTime() },
            w.0,
            l.0,
            context.as_raw() as usize,
        );
        if !consume && self.shared.notified.get() != Some(signature) {
            self.process_key(context, w, l, true)?;
            self.shared.notified.set(Some(signature));
        }
        Ok(consume.into())
    }
    fn OnTestKeyUp(&self, _: Ref<ITfContext>, w: WPARAM, _: LPARAM) -> Result<BOOL> {
        Ok(self
            .shared
            .eaten_keys
            .try_borrow()
            .is_ok_and(|keys| keys.contains(&w.0))
            .into())
    }
    fn OnKeyDown(&self, context: Ref<ITfContext>, w: WPARAM, l: LPARAM) -> Result<BOOL> {
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
        let signature = (
            unsafe { windows::Win32::UI::WindowsAndMessaging::GetMessageTime() },
            w.0,
            l.0,
            context.as_raw() as usize,
        );
        if self.shared.notified.get() == Some(signature) {
            return Ok(false.into());
        }
        let consumed = self.process_key(context, w, l, false)?;
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
        passed: bool,
    ) -> Result<bool> {
        let _ = refresh_scope(&self.shared, context);
        if self.shared.password.get() {
            self.shared.suspend_secure();
            return Ok(false);
        }
        let Some(key) = translate(w, l, false) else {
            return Ok(false);
        };
        self.shared.connect();
        if let Ok(mut saved) = self.shared.context.try_borrow_mut() {
            *saved = Some(context.clone());
        }
        let client = self
            .shared
            .client
            .try_borrow_mut()
            .ok()
            .and_then(|mut c| c.take());
        let Some(mut client) = client else {
            return Ok(false);
        };
        let observed = self
            .shared
            .observed
            .try_borrow()
            .map(|c| c.clone())
            .unwrap_or_default();
        self.shared.stop_refinement();
        let consumed = client.key(
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
        if let Ok(mut c) = self.shared.client.try_borrow_mut() {
            *c = Some(client);
        }
        if held {
            self.shared.hide();
        }
        Ok(consumed)
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
    fn OnCompositionTerminated(&self, _: u32, _: Ref<ITfComposition>) -> Result<()> {
        self.shared.reset();
        if let Ok(mut c) = self.shared.composition.try_borrow_mut() {
            c.take();
        }
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
    fn OnCompositionTerminated(&self, _: u32, _: Ref<ITfComposition>) -> Result<()> {
        if let Some(s) = self.shared.upgrade() {
            s.hide();
            if let Ok(mut c) = s.composition.try_borrow_mut() {
                c.take();
            }
        }
        Ok(())
    }
}
struct NativeDocument {
    shared: Rc<Shared>,
    context: ITfContext,
}
impl Document for NativeDocument {
    fn apply(&mut self, reply: &KeyReply) -> bool {
        if self.shared.password.get() {
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
        let success = unsafe {
            self.context.RequestEditSession(
                self.shared.tid.get(),
                &edit,
                TF_ES_SYNC | TF_ES_READWRITE,
            )
        }
        .is_ok_and(|hr| hr.is_ok());
        if success {
            if let Some(page) = &reply.candidates {
                self.shared.show(page);
            } else {
                self.shared.hide();
            }
        }
        success
    }
    fn finish_reading(&mut self, reading: &str) -> bool {
        self.apply(&KeyReply {
            preedit: Some(Preedit::default()),
            commit: Some(ipc::Commit {
                reading: reading.into(),
                value: reading.into(),
                synthetic: true,
                transient: false,
                words: vec![],
            }),
            ..Default::default()
        })
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
            write_composition(&self.shared, &self.context, cookie, &text, false)?;
            if let Some(composition) = self
                .shared
                .composition
                .try_borrow()
                .ok()
                .and_then(|c| c.clone())
            {
                let range = unsafe { composition.GetRange()? };
                let caret = unsafe { range.Clone()? };
                unsafe {
                    caret.Collapse(cookie, TF_ANCHOR_START)?;
                }
                let mut shifted = 0;
                unsafe {
                    caret.ShiftStart(
                        cookie,
                        preedit.cursor_utf16 as i32,
                        &mut shifted,
                        std::ptr::null(),
                    )?;
                    caret.Collapse(cookie, TF_ANCHOR_START)?;
                }
                set_caret(&self.context, cookie, caret)?;
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
            }
        } else if self.reply.commit.is_none()
            && let Some(composition) = self
                .shared
                .composition
                .try_borrow_mut()
                .ok()
                .and_then(|mut c| c.take())
        {
            unsafe {
                let range = composition.GetRange()?;
                range.SetText(cookie, 0, &[])?;
                composition.EndComposition(cookie)?;
            }
        }
        if let Ok(mut current) = self.shared.preedit.try_borrow_mut() {
            *current = preedit;
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
) -> Result<()> {
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
        let c = unsafe { owner.StartComposition(cookie, &range, &observer)? };
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
        // SetText can pull the host's selection back to the start of the
        // replaced text. Keep the endpoint across EndComposition.
        let caret = unsafe { range.Clone()? };
        unsafe {
            caret.Collapse(cookie, TF_ANCHOR_END)?;
            composition.EndComposition(cookie)?;
        }
        if let Ok(mut slot) = shared.composition.try_borrow_mut() {
            slot.take();
        }
        set_caret(context, cookie, caret)?;
    }
    Ok(())
}
#[implement(ITfEditSession)]
struct ScopeEdit {
    shared: Rc<Shared>,
    context: ITfContext,
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
        self.shared.password.set(false);
        if unsafe { self.context.GetStatus() }.is_ok_and(|s| s.dwDynamicFlags & TS_SD_READONLY != 0)
        {
            self.shared.password.set(true);
            return Ok(());
        }
        {
            if let Ok(property) = unsafe { self.context.GetAppProperty(&GUID_PROP_INPUTSCOPE) }
                && let Ok(value) = unsafe { property.GetValue(cookie, &range) }
                && let Ok(unknown) = IUnknown::try_from(&value)
                && let Ok(scope) = unknown.cast::<ITfInputScope>()
            {
                let mut scopes = std::ptr::null_mut();
                let mut len = 0;
                if unsafe { scope.GetInputScopes(&mut scopes, &mut len) }.is_ok() {
                    if !scopes.is_null() && len <= 256 {
                        self.shared.password.set(
                            unsafe { std::slice::from_raw_parts(scopes, len as usize) }
                                .contains(&IS_PASSWORD),
                        );
                    }
                    unsafe {
                        CoTaskMemFree(Some(scopes.cast()));
                    }
                }
            }
        }
        let mut app_buffer = [0u16; 32768];
        let len = unsafe { GetModuleFileNameW(None, &mut app_buffer) } as usize;
        let app = app_buffer
            .get(..len)
            .and_then(|b| String::from_utf16(b).ok())
            .unwrap_or_default();
        let privacy = self
            .shared
            .client
            .try_borrow()
            .ok()
            .and_then(|c| c.as_ref().map(|c| c.privacy_for_app(&app)))
            .unwrap_or(Privacy::Unknown);
        let mut observed = ipc::Context {
            privacy: privacy as i32,
            app,
            ..Default::default()
        };
        if self.read_text && privacy == Privacy::Normal && !self.shared.password.get() {
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
        if let Ok(mut current) = self.shared.observed.try_borrow_mut() {
            *current = observed;
        }
        Ok(())
    }
}
fn refresh_scope(shared: &Rc<Shared>, context: &ITfContext) -> Result<()> {
    refresh_scope_with_text(shared, context, true)
}
fn refresh_scope_with_text(
    shared: &Rc<Shared>,
    context: &ITfContext,
    read_text: bool,
) -> Result<()> {
    shared.password.set(true);
    let edit: ITfEditSession = ScopeEdit {
        shared: shared.clone(),
        context: context.clone(),
        read_text,
        _guard: ModuleGuard::new(),
    }
    .into();
    let result =
        unsafe { context.RequestEditSession(shared.tid.get(), &edit, TF_ES_SYNC | TF_ES_READ) }
            .and_then(|status| status.ok());
    if result.is_err() {
        shared.password.set(true);
    }
    result
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
        if (0x30..=0x5a).contains(&vk) || (0xba..=0xe2).contains(&vk) {
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
