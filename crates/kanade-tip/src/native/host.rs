//! A TSF manager and ACP store on the test thread. No profile is installed,
//! focused or selected. The key sink opens only the test's own endpoint.
use super::*;
use kanade_ipc::{
    frontend::Transport,
    windows::{Listener, Program, instance_endpoint},
};
use windows::{
    Win32::{System::Variant::VARIANT, UI::WindowsAndMessaging::*},
    core::{BSTR, PCWSTR, PWSTR},
};
use windows_core::ComObject;

/// The input scope reported by the test store.
#[implement(ITfInputScope)]
struct Scope {
    kind: InputScope,
    fails: bool,
}
impl ITfInputScope_Impl for Scope_Impl {
    fn GetInputScopes(&self, scopes: *mut *mut InputScope, count: *mut u32) -> Result<()> {
        if self.fails {
            return Err(failure());
        }
        unsafe {
            let one = CoTaskMemAlloc(std::mem::size_of::<InputScope>()).cast::<InputScope>();
            if one.is_null() {
                return Err(E_OUTOFMEMORY.into());
            }
            one.write(self.kind);
            *scopes = one;
            *count = 1;
        }
        Ok(())
    }
    fn GetPhrase(&self, _: *mut *mut BSTR, _: *mut u32) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetRegularExpression(&self) -> Result<BSTR> {
        Err(E_NOTIMPL.into())
    }
    fn GetSRGS(&self) -> Result<BSTR> {
        Err(E_NOTIMPL.into())
    }
    fn GetXML(&self) -> Result<BSTR> {
        Err(E_NOTIMPL.into())
    }
}

#[implement(ITextStoreACP, ITfContextOwnerCompositionSink)]
struct Store {
    text: RefCell<Vec<u16>>,
    selection: Cell<(i32, i32)>,
    sink: RefCell<Option<ITextStoreACPSink>>,
    lock: Cell<u32>,
    scope: Cell<InputScope>,
    scope_asked: Cell<bool>,
    scope_fails: Cell<bool>,
    empty_scope: Cell<bool>,
    reject_lock_after: Cell<Option<u32>>,
    no_layout: Cell<bool>,
    busy: Cell<bool>,
    readonly: Cell<bool>,
}

impl Store {
    fn end(&self) -> i32 {
        self.text.borrow().len() as i32
    }
    fn require_write(&self) -> Result<()> {
        if self.lock.get() != TS_LF_READWRITE.0 {
            return Err(TS_E_NOLOCK.into());
        }
        Ok(())
    }
    fn ask(&self, count: u32, attributes: *const GUID) {
        let attrs = if attributes.is_null() {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(attributes, count as usize) }
        };
        self.scope_asked.set(attrs.contains(&GUID_PROP_INPUTSCOPE));
    }
    /// Replaces a part of the text and moves the selection with it.
    fn replace(&self, start: i32, end: i32, new: &[u16]) -> Result<TS_TEXTCHANGE> {
        self.require_write()?;
        let len = self.end();
        if start < 0 || start > end || end > len {
            return Err(TS_E_INVALIDPOS.into());
        }
        self.text
            .borrow_mut()
            .splice(start as usize..end as usize, new.iter().copied());
        let new_end = start + new.len() as i32;
        let moved = |position: i32| {
            if position <= start {
                position
            } else if position >= end {
                position + new_end - end
            } else {
                new_end
            }
        };
        let (from, to) = self.selection.get();
        self.selection.set((moved(from), moved(to)));
        Ok(TS_TEXTCHANGE {
            acpStart: start,
            acpOldEnd: end,
            acpNewEnd: new_end,
        })
    }
}

fn units(text: &PCWSTR, len: u32) -> &[u16] {
    if text.is_null() || len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(text.0, len as usize) }
    }
}

impl ITfContextOwnerCompositionSink_Impl for Store_Impl {
    fn OnStartComposition(&self, _: Ref<ITfCompositionView>) -> Result<BOOL> {
        Ok(true.into())
    }
    fn OnUpdateComposition(&self, _: Ref<ITfCompositionView>, _: Ref<ITfRange>) -> Result<()> {
        Ok(())
    }
    fn OnEndComposition(&self, _: Ref<ITfCompositionView>) -> Result<()> {
        Ok(())
    }
}

impl ITextStoreACP_Impl for Store_Impl {
    fn AdviseSink(&self, _: *const GUID, sink: Ref<IUnknown>, _: u32) -> Result<()> {
        *self.sink.borrow_mut() = Some(sink.ok()?.cast()?);
        Ok(())
    }
    fn UnadviseSink(&self, _: Ref<IUnknown>) -> Result<()> {
        self.sink.borrow_mut().take();
        Ok(())
    }
    fn RequestLock(&self, flags: u32) -> Result<HRESULT> {
        if let Some(remaining) = self.reject_lock_after.get() {
            self.reject_lock_after.set(remaining.checked_sub(1));
            if remaining == 0 {
                return Ok(TS_E_SYNCHRONOUS);
            }
        }
        if self.busy.get() || self.lock.get() != 0 {
            return Ok(TS_E_SYNCHRONOUS);
        }
        let sink = self.sink.borrow().clone().ok_or_else(failure)?;
        self.lock.set(flags & TS_LF_READWRITE.0);
        let result = unsafe { sink.OnLockGranted(TEXT_STORE_LOCK_FLAGS(self.lock.get())) };
        self.lock.set(0);
        Ok(result.map_or_else(|err| err.code(), |()| S_OK))
    }
    fn GetStatus(&self) -> Result<TS_STATUS> {
        Ok(TS_STATUS {
            dwDynamicFlags: if self.readonly.get() {
                TS_SD_READONLY
            } else {
                0
            },
            dwStaticFlags: TS_SS_NOHIDDENTEXT,
        })
    }
    fn QueryInsert(
        &self,
        start: i32,
        end: i32,
        _: u32,
        result_start: *mut i32,
        result_end: *mut i32,
    ) -> Result<()> {
        unsafe {
            *result_start = start;
            *result_end = end;
        }
        Ok(())
    }
    fn GetSelection(
        &self,
        _: u32,
        count: u32,
        selection: *mut TS_SELECTION_ACP,
        fetched: *mut u32,
    ) -> Result<()> {
        unsafe {
            *fetched = 0;
            if count > 0 {
                let (start, end) = self.selection.get();
                *selection = TS_SELECTION_ACP {
                    acpStart: start,
                    acpEnd: end,
                    style: TS_SELECTIONSTYLE {
                        ase: TS_AE_END,
                        fInterimChar: false.into(),
                    },
                };
                *fetched = 1;
            }
        }
        Ok(())
    }
    fn SetSelection(&self, count: u32, selection: *const TS_SELECTION_ACP) -> Result<()> {
        self.require_write()?;
        if count > 0 {
            let selection = unsafe { *selection };
            self.selection.set((selection.acpStart, selection.acpEnd));
        }
        Ok(())
    }
    fn GetText(
        &self,
        start: i32,
        end: i32,
        plain: PWSTR,
        plain_len: u32,
        plain_read: *mut u32,
        runs: *mut TS_RUNINFO,
        runs_len: u32,
        runs_read: *mut u32,
        next: *mut i32,
    ) -> Result<()> {
        let text = self.text.borrow();
        let len = text.len() as i32;
        if start < 0 || start > len {
            return Err(TS_E_INVALIDPOS.into());
        }
        // An end of -1 is the end of the document.
        let end = if end < start { len } else { end.min(len) };
        let mut count = (end - start) as u32;
        if plain_len > 0 {
            count = count.min(plain_len);
        }
        unsafe {
            if !plain.is_null() && plain_len > 0 {
                let from = text.as_ptr().add(start as usize);
                std::ptr::copy_nonoverlapping(from, plain.0, count as usize);
            }
            if !plain_read.is_null() {
                *plain_read = count;
            }
            if !runs_read.is_null() {
                *runs_read = 0;
            }
            if !runs.is_null() && runs_len > 0 && count > 0 {
                *runs = TS_RUNINFO {
                    uCount: count,
                    r#type: TS_RT_PLAIN,
                };
                *runs_read = 1;
            }
            if !next.is_null() {
                *next = start + count as i32;
            }
        }
        Ok(())
    }
    fn SetText(
        &self,
        _: u32,
        start: i32,
        end: i32,
        text: &PCWSTR,
        len: u32,
    ) -> Result<TS_TEXTCHANGE> {
        self.replace(start, end, units(text, len))
    }
    fn GetFormattedText(&self, _: i32, _: i32) -> Result<IDataObject> {
        Err(E_NOTIMPL.into())
    }
    fn GetEmbedded(&self, _: i32, _: *const GUID, _: *const GUID) -> Result<IUnknown> {
        Err(E_NOTIMPL.into())
    }
    fn QueryInsertEmbedded(&self, _: *const GUID, _: *const FORMATETC) -> Result<BOOL> {
        Ok(false.into())
    }
    fn InsertEmbedded(&self, _: u32, _: i32, _: i32, _: Ref<IDataObject>) -> Result<TS_TEXTCHANGE> {
        Err(E_NOTIMPL.into())
    }
    fn InsertTextAtSelection(
        &self,
        flags: u32,
        text: &PCWSTR,
        len: u32,
        start: *mut i32,
        end: *mut i32,
        change: *mut TS_TEXTCHANGE,
    ) -> Result<()> {
        let (from, to) = self.selection.get();
        unsafe {
            if flags & TS_IAS_QUERYONLY != 0 {
                *start = from;
                *end = to;
                return Ok(());
            }
            let done = self.replace(from, to, units(text, len))?;
            self.selection.set((done.acpNewEnd, done.acpNewEnd));
            if flags & TS_IAS_NOQUERY == 0 {
                *start = done.acpStart;
                *end = done.acpNewEnd;
            }
            if !change.is_null() {
                *change = done;
            }
        }
        Ok(())
    }
    fn InsertEmbeddedAtSelection(
        &self,
        _: u32,
        _: Ref<IDataObject>,
        _: *mut i32,
        _: *mut i32,
        _: *mut TS_TEXTCHANGE,
    ) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn RequestSupportedAttrs(&self, _: u32, count: u32, attributes: *const GUID) -> Result<()> {
        // TSF asks for the input scope of the document this way.
        self.ask(count, attributes);
        Ok(())
    }
    fn RequestAttrsAtPosition(
        &self,
        _: i32,
        count: u32,
        attributes: *const GUID,
        _: u32,
    ) -> Result<()> {
        self.ask(count, attributes);
        Ok(())
    }
    fn RequestAttrsTransitioningAtPosition(
        &self,
        _: i32,
        _: u32,
        _: *const GUID,
        _: u32,
    ) -> Result<()> {
        Ok(())
    }
    fn FindNextAttrTransition(
        &self,
        _: i32,
        halt: i32,
        _: u32,
        _: *const GUID,
        _: u32,
        next: *mut i32,
        found: *mut BOOL,
        offset: *mut i32,
    ) -> Result<()> {
        unsafe {
            *next = halt;
            *found = false.into();
            *offset = 0;
        }
        Ok(())
    }
    fn RetrieveRequestedAttrs(
        &self,
        count: u32,
        values: *mut TS_ATTRVAL,
        fetched: *mut u32,
    ) -> Result<()> {
        unsafe {
            *fetched = 0;
            if self.scope_asked.replace(false) && count > 0 {
                let scope: ITfInputScope = Scope {
                    fails: self.scope_fails.get(),
                    kind: self.scope.get(),
                }
                .into();
                values.write(TS_ATTRVAL {
                    idAttr: GUID_PROP_INPUTSCOPE,
                    dwOverlapId: 0,
                    varValue: if self.empty_scope.get() {
                        VARIANT::default()
                    } else {
                        VARIANT::from(IUnknown::from(scope))
                    },
                });
                *fetched = 1;
            }
        }
        Ok(())
    }
    fn GetEndACP(&self) -> Result<i32> {
        Ok(self.end())
    }
    fn GetActiveView(&self) -> Result<u32> {
        Ok(1)
    }
    fn GetACPFromPoint(&self, _: u32, _: *const POINT, _: u32) -> Result<i32> {
        Err(E_NOTIMPL.into())
    }
    fn GetTextExt(
        &self,
        _: u32,
        _: i32,
        _: i32,
        rect: *mut RECT,
        clipped: *mut BOOL,
    ) -> Result<()> {
        if self.no_layout.get() {
            return Err(TS_E_NOLAYOUT.into());
        }
        unsafe {
            *rect = RECT {
                left: 10,
                top: 10,
                right: 20,
                bottom: 30,
            };
            *clipped = false.into();
        }
        Ok(())
    }
    fn GetScreenExt(&self, _: u32) -> Result<RECT> {
        Ok(RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 600,
        })
    }
    fn GetWnd(&self, _: u32) -> Result<HWND> {
        Ok(HWND::default())
    }
}

thread_local! {
    static ENDPOINT: RefCell<Option<String>> = const { RefCell::new(None) };
    static OPENS: Cell<usize> = const { Cell::new(0) };
    static START_PROBES: Cell<usize> = const { Cell::new(0) };
    static HINTS: Cell<usize> = const { Cell::new(0) };
    static LAST_HINT: Cell<HWND> = Cell::new(HWND::default());
    static CLOSE_AFTER_PROBE: Cell<bool> = const { Cell::new(false) };
    static HELLOS: Cell<usize> = const { Cell::new(0) };
}

/// A real pipe whose peer can disappear immediately after a successful probe.
pub(super) struct TestPipe(Option<Pipe>);

impl TestPipe {
    pub(super) fn new(pipe: Pipe) -> Self {
        Self(Some(pipe))
    }

    fn pipe(&mut self) -> std::io::Result<&mut Pipe> {
        self.0
            .as_mut()
            .ok_or(std::io::ErrorKind::NotConnected.into())
    }
}

impl Transport for TestPipe {
    fn is_open(&mut self) -> bool {
        let open = self.0.as_ref().is_some_and(Pipe::peer_open);
        if CLOSE_AFTER_PROBE.replace(false) {
            self.0.take();
        }
        open
    }
    fn exchange(&mut self, request: &ipc::Request) -> std::io::Result<ipc::Response> {
        if matches!(request.body, Some(ipc::request::Body::Hello(_))) {
            HELLOS.set(HELLOS.get() + 1);
        }
        self.pipe()?.exchange(request)
    }
    fn notify(&mut self, request: &ipc::Request) -> std::io::Result<()> {
        self.pipe()?.notify(request)
    }
}

pub(super) fn hinted(window: HWND) {
    if unsafe { IsWindowVisible(window) }.as_bool() {
        HINTS.set(HINTS.get() + 1);
        LAST_HINT.set(window);
    }
}

fn open_pipe(allow_start: bool) -> Option<Pipe> {
    OPENS.set(OPENS.get() + 1);
    if allow_start {
        START_PROBES.set(START_PROBES.get() + 1);
    }
    let name = ENDPOINT.with_borrow(Clone::clone)?;
    Pipe::connect_to(&name, &Program::Win32(std::env::current_exe().ok()?)).ok()
}

struct Host {
    tip: ComObject<Tip>,
    owner_id: u32,
    context: ITfContext,
    store: ComObject<Store>,
}

impl Host {
    fn test(&self, vk: usize) -> Result<bool> {
        let keys: ITfKeyEventSink = self.tip.to_interface();
        Ok(unsafe { keys.OnTestKeyDown(&self.context, WPARAM(vk), LPARAM(1))? }.as_bool())
    }

    fn down(&self, vk: usize) -> Result<bool> {
        let keys: ITfKeyEventSink = self.tip.to_interface();
        Ok(unsafe { keys.OnKeyDown(&self.context, WPARAM(vk), LPARAM(1))? }.as_bool())
    }

    fn check_up(&self, vk: usize, taken: bool) -> Result<()> {
        let keys: ITfKeyEventSink = self.tip.to_interface();
        unsafe {
            assert_eq!(
                keys.OnTestKeyUp(&self.context, WPARAM(vk), LPARAM(0))?
                    .as_bool(),
                taken
            );
            // WPF delivers KeyUp only when TestKeyUp requested it.
            if taken {
                assert!(
                    keys.OnKeyUp(&self.context, WPARAM(vk), LPARAM(0))?
                        .as_bool()
                );
            }
            assert!(
                !keys
                    .OnTestKeyUp(&self.context, WPARAM(vk), LPARAM(0))?
                    .as_bool()
            );
        }
        Ok(())
    }
}

fn with_host(check: impl FnOnce(&Host) -> Result<()>) -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
    let result = (|| unsafe {
        let manager: ITfThreadMgr =
            CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
        let mut id = 0;
        manager
            .cast::<ITfThreadMgrEx>()?
            .ActivateEx(&mut id, TF_TMAE_NOACTIVATETIP)?;
        let documents = manager.CreateDocumentMgr()?;
        let store = ComObject::new(Store {
            text: RefCell::new(vec![65, 66]),
            selection: Cell::new((2, 2)),
            sink: RefCell::new(None),
            lock: Cell::new(0),
            scope: Cell::new(IS_DEFAULT),
            scope_asked: Cell::new(false),
            scope_fails: Cell::new(false),
            empty_scope: Cell::new(false),
            reject_lock_after: Cell::new(None),
            no_layout: Cell::new(false),
            busy: Cell::new(false),
            readonly: Cell::new(false),
        });
        let mut context = None;
        let mut cookie = 0;
        documents.CreateContext(
            id,
            0,
            &store.to_interface::<ITextStoreACP>(),
            &mut context,
            &mut cookie,
        )?;
        let context = context.ok_or_else(failure)?;
        documents.Push(&context)?;
        let mut tip = Tip::new();
        Rc::get_mut(&mut tip.shared).ok_or_else(failure)?.open_pipe = open_pipe;
        tip.shared
            .tid
            .set(manager.cast::<ITfClientId>()?.GetClientId(&CLSID)?);
        tip.japanese.set(true);
        let host = Host {
            tip: ComObject::new(tip),
            owner_id: id,
            context,
            store,
        };
        let mut keyboard = [0; 256];
        GetKeyboardState(&mut keyboard)?;
        SetKeyboardState(&[0; 256])?;
        // The tooltip needs a focus owner on this thread.
        let window = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            windows::core::w!("STATIC"),
            None,
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            None,
            None,
        )?;
        let _ = SetFocus(Some(window));
        assert!(GetFocus() == window);
        OPENS.set(0);
        HINTS.set(0);
        START_PROBES.set(0);
        CLOSE_AFTER_PROBE.set(false);
        HELLOS.set(0);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&host)));
        ENDPOINT.with_borrow_mut(Option::take);
        SetKeyboardState(&keyboard)?;
        drop(host);
        DestroyWindow(window)?;
        documents.Pop(TF_POPF_ALL)?;
        manager.Deactivate()?;
        match result {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })();
    unsafe { CoUninitialize() };
    result
}

// The top row includes ASCII starters even on non-Latin keyboard layouts.
fn starter() -> Result<usize> {
    (0x41..=0x5a)
        .chain(0x30..=0x39)
        .find(|&vk| {
            translate(WPARAM(vk), LPARAM(1), false)
                .is_some_and(|k| !k.text.is_empty() && k.text.is_ascii())
        })
        .ok_or_else(failure)
}

#[test]
fn outage_drops_starters_once_and_leaves_backspace_to_the_application() -> Result<()> {
    with_host(|host| {
        let vk = starter()?;
        for i in 0..3 {
            assert!(host.test(vk)?);
            assert!(host.test(vk)?);
            assert_eq!(HINTS.get(), usize::from(i != 0));
            assert!(host.down(vk)?);
            host.check_up(vk, true)?;
        }
        assert_eq!(HINTS.get(), 1);
        assert_eq!(host.store.end(), 2);
        for vk in [8, 0x0d, 0x25, 0x20, 9, 0x1b, 0x60] {
            assert!(!host.test(vk)?);
            assert!(!host.down(vk)?);
            if vk == 8 {
                host.store.text.borrow_mut().pop();
                host.store.selection.set((1, 1));
            }
            host.check_up(vk, false)?;
        }
        assert_eq!(host.store.end(), 1);
        assert_eq!(HINTS.get(), 1);
        assert_eq!(START_PROBES.get(), 3);
        Ok(())
    })
}

#[test]
fn protected_or_unreadable_field_passes_the_key_without_a_start_or_hint() -> Result<()> {
    for case in 0..6 {
        with_host(|host| {
            match case {
                0 => host.store.scope.set(IS_PASSWORD),
                1 => host.store.readonly.set(true),
                2 => host.store.busy.set(true),
                3 => host.tip.japanese.set(false),
                4 => host.tip.shared.secure.set(true),
                _ => host.store.scope_fails.set(true),
            }
            let vk = starter()?;
            assert!(!host.test(vk)?);
            assert!(!host.down(vk)?);
            host.check_up(vk, false)?;
            assert_eq!(START_PROBES.get(), 0);
            assert_eq!(HINTS.get(), 0);
            Ok(())
        })?;
    }
    Ok(())
}

#[test]
fn down_consumes_a_claim_without_sending_it_to_a_newly_protected_field() -> Result<()> {
    with_host(|host| {
        let vk = starter()?;
        assert!(host.test(vk)?);
        let opens = OPENS.get();
        host.store.scope.set(IS_PASSWORD);
        assert!(host.down(vk)?);
        host.check_up(vk, true)?;
        assert_eq!(OPENS.get(), opens);
        assert_eq!(HINTS.get(), 0);
        Ok(())
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reply {
    Commit,
    PassBackspace,
    Decline,
    Lose,
    Stall,
}

/// A single connection. Each step expects one physical key, including the
/// existing observation of a Backspace passed to the application.
fn listener() -> Result<Listener> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let name = instance_endpoint(&format!(
        "tip-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
    .map_err(|_| failure())?;
    let listener = Listener::bind_to(&name).map_err(|_| failure())?;
    ENDPOINT.with_borrow_mut(|endpoint| *endpoint = Some(name));
    Ok(listener)
}

fn serve(steps: &'static [Reply]) -> Result<std::thread::JoinHandle<std::io::Result<()>>> {
    let mut listener = listener()?;
    Ok(std::thread::spawn(move || {
        let mut pipe = listener
            .accept()
            .map_err(|_| std::io::Error::other("accept failed"))?;
        drop(listener);
        pipe.begin(ipc::KEY_TIMEOUT);
        let hello: ipc::Request = ipc::receive(&mut pipe)?;
        assert!(matches!(hello.body, Some(ipc::request::Body::Hello(_))));
        ipc::send(
            &mut pipe,
            &ipc::Response {
                body: Some(ipc::response::Body::Welcome(ipc::Welcome {
                    version: ipc::CURRENT_VERSION,
                    connection: 1,
                    ..Default::default()
                })),
            },
        )?;
        for (i, &step) in steps.iter().enumerate() {
            pipe.begin(ipc::KEY_TIMEOUT);
            let request: ipc::Request = ipc::receive(&mut pipe)?;
            let Some(ipc::request::Body::Key(key)) = request.body else {
                return Err(std::io::Error::other("expected key"));
            };
            assert_eq!(key.event_seq, i as u64 + 1);
            assert_eq!(key.passed_through, step == Reply::PassBackspace);
            if step == Reply::Lose {
                return Ok(());
            }
            if step == Reply::Stall {
                // The peer sends no reply. Its client must close on timeout.
                match pipe.read_idle_frame(ipc::FRAME_TIMEOUT) {
                    Ok(None) => (),
                    Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => (),
                    _ => return Err(std::io::Error::other("expected disconnect after timeout")),
                }
                return Ok(());
            }
            if step == Reply::PassBackspace {
                assert!(
                    key.key
                        .as_ref()
                        .is_some_and(|k| k.special == SpecialKey::Backspace as i32)
                );
            }
            ipc::send(
                &mut pipe,
                &ipc::Response {
                    body: Some(ipc::response::Body::Key(KeyReply {
                        revision: key.revision,
                        event_seq: key.event_seq,
                        consumed: step == Reply::Commit,
                        commit: (step == Reply::Commit).then(|| ipc::Commit {
                            value: "字".into(),
                            ..Default::default()
                        }),
                        key_policy: Some(ipc::KeyPolicySummary {
                            enabled: true,
                            ..Default::default()
                        }),
                        ..Default::default()
                    })),
                },
            )?;
            let request: ipc::Request = ipc::receive(&mut pipe)?;
            assert!(
                matches!(request.body, Some(ipc::request::Body::Ack(ack)) if ack.applied && ack.event_seq == key.event_seq)
            );
        }
        // The client closes the connection when the test is done.
        // Another request here would be a duplicate.
        assert!(
            pipe.read_idle_frame(ipc::FRAME_TIMEOUT)
                .ok()
                .flatten()
                .is_none()
        );
        Ok(())
    }))
}

fn finish(server: std::thread::JoinHandle<std::io::Result<()>>) -> Result<()> {
    server.join().map_err(|_| failure())?.map_err(|_| failure())
}

#[test]
fn recovery_between_test_and_down_delivers_one_key_and_one_document_edit() -> Result<()> {
    with_host(|host| {
        let vk = starter()?;
        assert!(host.test(vk)?);
        let server = serve(&[Reply::Commit, Reply::PassBackspace])?;
        assert!(host.test(vk)?);
        assert_eq!(START_PROBES.get(), 0);
        assert_eq!(HINTS.get(), 0);
        assert_eq!(host.store.end(), 2);
        assert!(host.down(vk)?);
        host.check_up(vk, true)?;
        assert!(*host.store.text.borrow() == [65, 66, '字' as u16]);
        // The app receives the key and deletes once; the peer sees one observation.
        assert!(!host.test(8)?);
        assert!(!host.test(8)?);
        assert!(!host.down(8)?);
        host.store.text.borrow_mut().pop();
        host.check_up(8, false)?;
        host.tip.shared.client.borrow_mut().take();
        finish(server)?;
        assert_eq!(host.store.end(), 2);
        assert_eq!(HINTS.get(), 0);
        Ok(())
    })
}

#[test]
fn a_recovered_session_cannot_return_an_already_claimed_key_to_the_app() -> Result<()> {
    with_host(|host| {
        let vk = starter()?;
        assert!(host.test(vk)?);
        let server = serve(&[Reply::Decline])?;
        // Another callback made the connection before Down.
        host.tip.shared.connect(false);
        assert!(host.down(vk)?);
        host.check_up(vk, true)?;
        host.tip.shared.client.borrow_mut().take();
        finish(server)?;
        assert_eq!(HINTS.get(), 0);
        assert_eq!(host.store.end(), 2);
        Ok(())
    })
}

#[test]
fn a_successful_key_rearms_the_hint_after_repeated_lost_replies() -> Result<()> {
    with_host(|host| {
        let vk = starter()?;
        for _ in 0..2 {
            let server = serve(&[Reply::Lose])?;
            assert!(host.test(vk)? && host.down(vk)?);
            host.check_up(vk, true)?;
            finish(server)?;
            assert_eq!(HINTS.get(), 1);
        }
        assert_eq!(host.store.end(), 2);
        let server = serve(&[Reply::Commit, Reply::Lose])?;
        let outage = LAST_HINT.get();
        assert!(host.test(vk)?);
        assert!(unsafe { IsWindowVisible(outage) }.as_bool());
        assert!(host.down(vk)?);
        host.check_up(vk, true)?;
        assert_eq!(host.store.end(), 3);
        assert!(!unsafe { IsWindowVisible(outage) }.as_bool());
        assert_eq!(HINTS.get(), 1);
        assert!(host.test(vk)? && host.down(vk)?);
        host.check_up(vk, true)?;
        finish(server)?;
        assert_eq!(HINTS.get(), 2);
        assert_eq!(host.store.end(), 3);
        Ok(())
    })
}

#[test]
fn first_key_after_focus_connects_even_if_the_selection_has_no_single_scope() -> Result<()> {
    with_host(|host| {
        let keys: ITfKeyEventSink = host.tip.to_interface();
        unsafe {
            keys.OnSetFocus(false)?;
            keys.OnSetFocus(true)?;
        }
        host.store.selection.set((0, 2));
        host.store.empty_scope.set(true);
        let server = serve(&[Reply::Commit])?;
        let vk = starter()?;
        assert!(host.test(vk)?);
        assert_eq!(HELLOS.get(), 1);
        assert!(host.down(vk)?);
        host.check_up(vk, true)?;
        assert!(*host.store.text.borrow() == ['字' as u16]);
        host.tip.shared.client.borrow_mut().take();
        finish(server)
    })
}

#[test]
fn disconnect_after_the_test_probe_still_consumes_the_starter() -> Result<()> {
    with_host(|host| {
        let server = serve(&[])?;
        host.tip.shared.connect(false);
        ENDPOINT.with_borrow_mut(Option::take);
        CLOSE_AFTER_PROBE.set(true);
        let vk = starter()?;
        assert!(host.test(vk)?);
        assert!(host.down(vk)?);
        host.check_up(vk, true)?;
        assert_eq!(host.store.end(), 2);
        assert_eq!(HINTS.get(), 1);
        finish(server)
    })
}

#[test]
fn a_rejected_scope_refresh_after_reconnect_still_consumes_the_claim() -> Result<()> {
    with_host(|host| {
        let vk = starter()?;
        assert!(host.test(vk)?);
        let server = serve(&[])?;
        host.store.reject_lock_after.set(Some(1));
        assert!(host.down(vk)?);
        host.check_up(vk, true)?;
        assert_eq!(HELLOS.get(), 1);
        assert_eq!(host.store.end(), 2);
        host.tip.shared.client.borrow_mut().take();
        finish(server)
    })
}

#[test]
fn skipped_down_and_unclaimed_key_up_leave_no_claim_on_the_next_press() -> Result<()> {
    with_host(|host| {
        let vk = starter()?;
        assert!(host.test(vk)?);
        host.check_up(vk, false)?;
        let server = serve(&[Reply::Decline])?;
        host.tip.shared.connect(false);
        assert!(host.test(vk)?);
        assert!(!host.down(vk)?);
        host.check_up(vk, false)?;
        host.tip.shared.client.borrow_mut().take();
        finish(server)
    })
}

#[test]
fn a_repeat_without_test_cannot_use_the_first_messages_claim() -> Result<()> {
    with_host(|host| {
        let vk = starter()?;
        assert!(host.test(vk)?);
        let server = serve(&[Reply::Decline])?;
        host.tip.shared.connect(false);
        let keys: ITfKeyEventSink = host.tip.to_interface();
        let repeat = LPARAM((1 << 30) | 1);
        assert!(!unsafe { keys.OnKeyDown(&host.context, WPARAM(vk), repeat)? }.as_bool());
        host.check_up(vk, false)?;
        host.tip.shared.client.borrow_mut().take();
        finish(server)
    })
}

#[test]
fn connected_starter_claim_survives_a_denied_edit_lock_on_down() -> Result<()> {
    with_host(|host| {
        let server = serve(&[])?;
        host.tip.shared.connect(false);
        let vk = starter()?;
        assert!(host.test(vk)?);
        host.store.busy.set(true);
        assert!(host.down(vk)?);
        host.check_up(vk, true)?;
        assert_eq!(host.store.end(), 2);
        finish(server)
    })
}

#[test]
fn replacing_the_peer_on_down_keeps_the_tested_starter_claim() -> Result<()> {
    with_host(|host| {
        let first = serve(&[])?;
        host.tip.shared.connect(false);
        CLOSE_AFTER_PROBE.set(true);
        let vk = starter()?;
        assert!(host.test(vk)?);
        let second = serve(&[Reply::Decline])?;
        assert!(host.down(vk)?);
        host.check_up(vk, true)?;
        host.tip.shared.client.borrow_mut().take();
        finish(first)?;
        finish(second)
    })
}

#[test]
fn down_without_test_returns_the_new_peers_decline() -> Result<()> {
    with_host(|host| {
        let server = serve(&[Reply::Decline])?;
        let vk = starter()?;
        assert!(!host.down(vk)?);
        host.check_up(vk, false)?;
        assert_eq!(host.store.end(), 2);
        assert_eq!(HINTS.get(), 0);
        host.tip.shared.client.borrow_mut().take();
        finish(server)
    })
}

#[test]
fn key_timeout_does_not_start_a_second_handshake() -> Result<()> {
    with_host(|host| {
        let server = serve(&[Reply::Stall])?;
        host.tip.shared.connect(false);
        let vk = starter()?;
        assert!(host.test(vk)?);
        // A fresh pipe accepts opens but never answers Hello.
        let _silent = listener()?;
        assert!(host.down(vk)?);
        assert_eq!(HELLOS.get(), 1);
        host.check_up(vk, true)?;
        finish(server)
    })
}

#[test]
fn timed_out_test_handshake_is_not_repeated_on_delivery() -> Result<()> {
    with_host(|host| {
        let _silent = listener()?;
        let vk = starter()?;
        assert!(host.test(vk)?);
        assert_eq!(HELLOS.get(), 1);
        assert!(host.test(vk)?);
        assert!(host.down(vk)?);
        assert_eq!(HELLOS.get(), 1);
        host.check_up(vk, true)?;
        Ok(())
    })
}

#[test]
fn failed_hint_is_retried_until_displayed() -> Result<()> {
    with_host(|host| {
        let vk = starter()?;
        host.store.no_layout.set(true);
        for expected in [0, 1, 1] {
            assert!(host.test(vk)? && host.down(vk)?);
            host.check_up(vk, true)?;
            assert_eq!(HINTS.get(), expected);
            host.store.no_layout.set(false);
        }
        Ok(())
    })
}

fn check_interrupted_outage_hint(interrupt: impl FnOnce(&Host) -> Result<()>) -> Result<()> {
    with_host(|host| {
        let vk = starter()?;
        assert!(host.down(vk)?);
        host.check_up(vk, true)?;
        assert_eq!(HINTS.get(), 1);
        let outage = LAST_HINT.get();
        interrupt(host)?;
        assert!(!unsafe { IsWindowVisible(outage) }.as_bool());
        let shown = HINTS.get();
        assert!(host.test(vk)? && host.down(vk)?);
        assert_eq!(HINTS.get(), shown + 1);
        assert!(unsafe { IsWindowVisible(LAST_HINT.get()) }.as_bool());
        Ok(())
    })
}

#[test]
fn focus_change_rearms_the_outage_hint() -> Result<()> {
    check_interrupted_outage_hint(|host| unsafe {
        let keys: ITfKeyEventSink = host.tip.to_interface();
        keys.OnSetFocus(false)?;
        keys.OnSetFocus(true)
    })
}

#[test]
fn context_push_rearms_the_outage_hint() -> Result<()> {
    check_interrupted_outage_hint(|host| unsafe {
        let sink: ITfThreadMgrEventSink = host.tip.to_interface();
        sink.OnPushContext(&host.context)
    })
}

#[test]
fn context_pop_rearms_the_outage_hint() -> Result<()> {
    check_interrupted_outage_hint(|host| unsafe {
        let sink: ITfThreadMgrEventSink = host.tip.to_interface();
        sink.OnPopContext(&host.context)
    })
}

#[test]
fn mode_indicator_rearms_the_outage_hint() -> Result<()> {
    check_interrupted_outage_hint(|host| {
        assert!(host.down(usize::from(VK_IME_ON.0))?);
        assert_eq!(HINTS.get(), 2);
        Ok(())
    })
}

#[test]
fn a_successful_key_keeps_the_mode_indicator_visible() -> Result<()> {
    with_host(|host| {
        let vk = starter()?;
        assert!(host.down(vk)?);
        assert!(host.down(usize::from(VK_IME_ON.0))?);
        assert_eq!(HINTS.get(), 2);
        let mode = LAST_HINT.get();
        let server = serve(&[Reply::Commit])?;
        assert!(host.test(vk)? && host.down(vk)?);
        assert_eq!(host.store.end(), 3);
        assert!(unsafe { IsWindowVisible(mode) }.as_bool());
        host.tip.shared.client.borrow_mut().take();
        finish(server)
    })
}

#[test]
fn outage_hint_stays_visible_longer_than_the_mode_indicator() -> Result<()> {
    with_host(|host| {
        let vk = starter()?;
        assert!(host.down(vk)?);
        assert_eq!(HINTS.get(), 1);
        let hwnd = LAST_HINT.get();
        std::thread::sleep(std::time::Duration::from_millis(1600));
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, Some(hwnd), WM_TIMER, WM_TIMER, PM_REMOVE).as_bool() {
                DispatchMessageW(&msg);
            }
            assert!(IsWindowVisible(hwnd).as_bool());
        }
        Ok(())
    })
}

#[test]
fn continued_outage_shows_another_hint_on_the_next_drop_after_ten_seconds() -> Result<()> {
    with_host(|host| {
        let vk = starter()?;
        assert!(host.down(vk)? && host.down(vk)?);
        assert_eq!(HINTS.get(), 1);
        std::thread::sleep(std::time::Duration::from_secs(10));
        assert!(!host.down(8)?);
        assert!(host.test(vk)?);
        assert_eq!(HINTS.get(), 1);
        assert!(host.down(vk)? && host.down(vk)?);
        assert_eq!(HINTS.get(), 2);
        Ok(())
    })
}

#[test]
fn secure_mode_uses_the_connected_engine_and_passes_keys_on_loss() -> Result<()> {
    with_host(|host| {
        host.tip.shared.secure.set(true);
        let vk = starter()?;
        let server = serve(&[Reply::Commit, Reply::PassBackspace, Reply::Lose])?;
        assert!(host.test(vk)? && host.down(vk)?);
        host.check_up(vk, true)?;
        assert_eq!(host.store.end(), 3);
        assert!(!host.test(8)? && !host.down(8)?);
        assert!(host.test(vk)?);
        assert!(!host.down(vk)?);
        host.check_up(vk, false)?;
        finish(server)?;
        assert!(!host.test(vk)? && !host.down(vk)?);
        assert!(!host.down(vk)?);
        assert_eq!(START_PROBES.get(), 0);
        assert_eq!(HINTS.get(), 0);
        Ok(())
    })
}

#[test]
fn uiless_host_consumes_an_offline_starter_without_a_tooltip() -> Result<()> {
    with_host(|host| {
        host.tip.show_mode_hint.set(false);
        let vk = starter()?;
        assert!(host.test(vk)? && host.down(vk)?);
        host.check_up(vk, true)?;
        assert_eq!(START_PROBES.get(), 1);
        assert_eq!(HINTS.get(), 0);
        Ok(())
    })
}

#[test]
fn host_can_show_a_caret_hint() -> Result<()> {
    with_host(|host| {
        let _hint = mode_hint::Hint::at_selection(&host.context, host.tip.shared.tid.get(), true)?;
        assert_eq!(HINTS.get(), 1);
        Ok(())
    })
}

#[test]
fn outage_uses_the_connected_starter_policy_in_host_reported_scopes() -> Result<()> {
    for (scope, disabled, drop_offline) in [
        (None, false, true),
        (Some(IS_DEFAULT), false, true),
        (Some(IS_NUMERIC_PASSWORD), false, false),
        (Some(IS_NUMERIC_PIN), false, false),
        (Some(IS_ALPHANUMERIC_PIN), false, false),
        (Some(IS_ALPHANUMERIC_PIN_SET), false, false),
        (Some(IS_DEFAULT), true, false),
    ] {
        with_host(|host| {
            host.store.empty_scope.set(scope.is_none());
            host.store.scope.set(scope.unwrap_or(IS_DEFAULT));
            if disabled {
                let compartments = host.context.cast::<ITfCompartmentMgr>()?;
                unsafe {
                    compartments
                        .GetCompartment(&GUID_COMPARTMENT_KEYBOARD_DISABLED)?
                        .SetValue(host.owner_id, &VARIANT::from(1i32))?;
                }
            }
            let vk = (0x30..=0x39)
                .find(|&vk| {
                    translate(WPARAM(vk), LPARAM(1), false)
                        .is_some_and(|key| ipc::frontend::starts_composition(&key))
                })
                .unwrap_or(starter()?);
            let server = serve(&[Reply::Commit, Reply::PassBackspace, Reply::Lose])?;
            assert!(host.test(vk)? && host.down(vk)?);
            host.check_up(vk, true)?;
            assert!(!host.test(8)?);
            host.check_up(8, false)?;
            assert_eq!(HINTS.get(), 0);
            assert_eq!(START_PROBES.get(), 0);

            // The peer disappears after Test, while handling this Down.
            assert!(host.test(vk)?);
            assert_eq!(host.down(vk)?, drop_offline);
            host.check_up(vk, drop_offline)?;
            finish(server)?;

            assert_eq!(host.test(vk)?, drop_offline);
            assert_eq!(host.down(vk)?, drop_offline);
            host.check_up(vk, drop_offline)?;
            assert_eq!(host.down(vk)?, drop_offline);
            host.check_up(vk, drop_offline)?;
            assert!(!host.test(8)?);
            host.check_up(8, false)?;
            assert_eq!(host.store.end(), 3);
            assert_eq!(HINTS.get(), usize::from(drop_offline));
            assert_eq!(START_PROBES.get(), 3 * usize::from(drop_offline));
            Ok(())
        })?;
    }
    Ok(())
}
