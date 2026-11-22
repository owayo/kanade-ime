//! Real TSF managers with isolated documents. The key sink uses an in-process
//! TIP and opens only the test's own endpoint.
use super::*;
use kanade_ipc::{
    frontend::Transport,
    windows::{Listener, Program, instance_endpoint},
};
use windows::{
    Win32::{
        System::Variant::{VARENUM, VARIANT, VT_EMPTY},
        UI::WindowsAndMessaging::*,
    },
    core::{BSTR, PCWSTR, PWSTR},
};
use windows_core::ComObject;

/// The input scope reported by the test store.
#[implement(ITfInputScope)]
struct Scope {
    kinds: Vec<InputScope>,
    fails: bool,
}
impl ITfInputScope_Impl for Scope_Impl {
    fn GetInputScopes(&self, scopes: *mut *mut InputScope, count: *mut u32) -> Result<()> {
        if self.fails {
            return Err(failure());
        }
        unsafe {
            *scopes = std::ptr::null_mut();
            *count = 0;
            if !self.kinds.is_empty() {
                let list = CoTaskMemAlloc(std::mem::size_of_val(self.kinds.as_slice()))
                    .cast::<InputScope>();
                if list.is_null() {
                    return Err(E_OUTOFMEMORY.into());
                }
                list.copy_from_nonoverlapping(self.kinds.as_ptr(), self.kinds.len());
                *scopes = list;
                *count = self.kinds.len() as u32;
            }
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
    scopes: RefCell<Vec<InputScope>>,
    scopes_asked: Cell<Option<Vec<InputScope>>>,
    scope_fails: Cell<bool>,
    supplies_scope: Cell<bool>,
    implements_attrs: Cell<bool>,
    value_error: Cell<Option<(GUID, HRESULT)>>,
    mixed_scope: Cell<bool>,
    supports_transitions: Cell<bool>,
    empty_scope: Cell<bool>,
    range_readonly: Cell<Option<bool>>,
    readonly_asked: Cell<Option<bool>>,
    preceding_reads: Cell<u32>,
    reject_lock_after: Cell<Option<u32>>,
    no_layout: Cell<bool>,
    busy: Cell<bool>,
    readonly: Cell<bool>,
}

impl Store {
    fn set_scope(&self, scope: InputScope) {
        self.scopes.replace(vec![scope]);
    }
    fn end(&self) -> i32 {
        self.text.borrow().len() as i32
    }
    fn require_write(&self) -> Result<()> {
        if self.lock.get() != TS_LF_READWRITE.0 {
            return Err(TS_E_NOLOCK.into());
        }
        Ok(())
    }
    fn ask(&self, position: Option<i32>, count: u32, attributes: *const GUID) -> Result<()> {
        if !self.implements_attrs.get() {
            return Err(E_NOTIMPL.into());
        }
        let attrs = if attributes.is_null() {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(attributes, count as usize) }
        };
        if position.is_some()
            && let Some((id, error)) = self.value_error.get()
            && attrs.contains(&id)
        {
            return Err(error.into());
        }
        self.scopes_asked.set(
            (self.supplies_scope.get() && attrs.contains(&GUID_PROP_INPUTSCOPE)).then(|| {
                if self.mixed_scope.get() && position.is_some_and(|pos| pos >= 2) {
                    vec![IS_PASSWORD]
                } else {
                    self.scopes.borrow().clone()
                }
            }),
        );
        self.readonly_asked.set(
            self.range_readonly
                .get()
                .filter(|_| attrs.contains(&TSATTRID_Text_ReadOnly))
                .map(|readonly| readonly && position.is_some_and(|pos| pos >= 2)),
        );
        Ok(())
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
                // TSF itself can read the selected or newly committed range.
                if start < end && end <= self.selection.get().0 && self.lock.get() == TS_LF_READ.0 {
                    self.preceding_reads.set(self.preceding_reads.get() + 1);
                }
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
        self.ask(None, count, attributes)
    }
    fn RequestAttrsAtPosition(
        &self,
        position: i32,
        count: u32,
        attributes: *const GUID,
        _: u32,
    ) -> Result<()> {
        self.ask(Some(position), count, attributes)
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
        start: i32,
        halt: i32,
        count: u32,
        attributes: *const GUID,
        _: u32,
        next: *mut i32,
        found: *mut BOOL,
        offset: *mut i32,
    ) -> Result<()> {
        if !self.supports_transitions.get() {
            return Err(E_NOTIMPL.into());
        }
        unsafe {
            let attrs = if attributes.is_null() {
                &[]
            } else {
                std::slice::from_raw_parts(attributes, count as usize)
            };
            let crosses = start.min(halt) < 2
                && start.max(halt) > 2
                && ((self.mixed_scope.get() && attrs.contains(&GUID_PROP_INPUTSCOPE))
                    || (self.range_readonly.get() == Some(true)
                        && attrs.contains(&TSATTRID_Text_ReadOnly)));
            *next = if crosses { 2 } else { halt };
            *found = crosses.into();
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
            if let Some(kinds) = self.scopes_asked.take()
                && count > 0
            {
                let scope: ITfInputScope = Scope {
                    fails: self.scope_fails.get(),
                    kinds,
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
            if let Some(value) = self.readonly_asked.take()
                && *fetched < count
            {
                values.add(*fetched as usize).write(TS_ATTRVAL {
                    idAttr: TSATTRID_Text_ReadOnly,
                    dwOverlapId: 0,
                    varValue: VARIANT::from(value),
                });
                *fetched += 1;
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
    owner_cookie: u32,
    context: ITfContext,
    store: ComObject<Store>,
}

impl Host {
    fn check_scope_value(&self, expected: std::result::Result<VARENUM, HRESULT>) -> Result<()> {
        unsafe {
            let start = self.context.GetStart(self.owner_cookie)?;
            let property = self.context.GetAppProperty(&GUID_PROP_INPUTSCOPE)?;
            let value = property.GetValue(self.owner_cookie, &start);
            assert_eq!(
                value
                    .map(|v| v.Anonymous.Anonymous.vt)
                    .map_err(|e| e.code()),
                expected
            );
        }
        Ok(())
    }

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

fn test_tip(manager: &ITfThreadMgr) -> Result<ComObject<Tip>> {
    let mut tip = Tip::new();
    Rc::get_mut(&mut tip.shared).ok_or_else(failure)?.open_pipe = open_pipe;
    tip.shared
        .tid
        .set(unsafe { manager.cast::<ITfClientId>()?.GetClientId(&CLSID)? });
    tip.japanese.set(true);
    Ok(ComObject::new(tip))
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
            scopes: RefCell::new(vec![IS_DEFAULT]),
            scopes_asked: Cell::new(None),
            scope_fails: Cell::new(false),
            supplies_scope: Cell::new(true),
            implements_attrs: Cell::new(true),
            value_error: Cell::new(None),
            mixed_scope: Cell::new(false),
            supports_transitions: Cell::new(true),
            empty_scope: Cell::new(false),
            range_readonly: Cell::new(None),
            readonly_asked: Cell::new(None),
            preceding_reads: Cell::new(0),
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
        let host = Host {
            tip: test_tip(&manager)?,
            owner_id: id,
            owner_cookie: cookie,
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
                0 => host.store.set_scope(IS_PASSWORD),
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
        host.store.set_scope(IS_PASSWORD);
        assert!(host.down(vk)?);
        host.check_up(vk, true)?;
        assert_eq!(OPENS.get(), opens);
        assert_eq!(HINTS.get(), 0);
        Ok(())
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reply {
    Compose,
    Commit,
    PrivateCommit,
    NormalCommit,
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
                    learning_policy: Some(ipc::LearningPolicy {
                        enabled: true,
                        ..Default::default()
                    }),
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
            if matches!(step, Reply::PrivateCommit | Reply::NormalCommit) {
                let observed = key
                    .context
                    .as_ref()
                    .ok_or_else(|| std::io::Error::other("missing field context"))?;
                let private = step == Reply::PrivateCommit;
                assert_eq!(
                    observed.privacy,
                    if private {
                        Privacy::Private
                    } else {
                        Privacy::Normal
                    } as i32
                );
                assert_eq!(observed.surrounding_available, !private);
                if private {
                    assert!(observed.preceding_text.is_empty());
                }
            }
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
            let commit = matches!(
                step,
                Reply::Commit | Reply::PrivateCommit | Reply::NormalCommit
            );
            ipc::send(
                &mut pipe,
                &ipc::Response {
                    body: Some(ipc::response::Body::Key(KeyReply {
                        revision: key.revision,
                        event_seq: key.event_seq,
                        consumed: commit || step == Reply::Compose,
                        preedit: (step == Reply::Compose).then(|| Preedit {
                            segments: vec![ipc::Segment {
                                reading: "じ".into(),
                                value: "じ".into(),
                                value_utf16: 1,
                                ..Default::default()
                            }],
                            cursor_utf16: 1,
                        }),
                        commit: commit.then(|| ipc::Commit {
                            value: "字".into(),
                            ..Default::default()
                        }),
                        key_policy: Some(ipc::KeyPolicySummary {
                            enabled: true,
                            composing: step == Reply::Compose,
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

// Start a TSF composition, then commit it. The peer checks both edit ACKs.
fn check_connected_commit(tip: &ComObject<Tip>, context: &ITfContext) -> Result<()> {
    let server = serve(&[Reply::Compose, Reply::Commit])?;
    assert!(tip.shared.connect(false) == Connection::Attached);
    let keys: ITfKeyEventSink = tip.to_interface();
    for (vk, composing) in [(starter()?, true), (0x0d, false)] {
        let key = WPARAM(vk);
        unsafe {
            assert!(keys.OnTestKeyDown(context, key, LPARAM(1))?.as_bool());
            assert!(keys.OnKeyDown(context, key, LPARAM(1))?.as_bool());
            assert!(keys.OnKeyUp(context, key, LPARAM(0))?.as_bool());
        }
        assert_eq!(tip.shared.composition.borrow().is_some(), composing);
    }
    tip.shared.client.borrow_mut().take();
    finish(server)
}

// An application can use TSF's own store and supply attributes through a
// context owner. Windows Terminal uses this model.
#[implement(ITfContextOwner, ITfContextOwnerCompositionSink)]
struct EmptyAttributeOwner {
    window: HWND,
    asked_for_scope: Cell<bool>,
}

impl ITfContextOwner_Impl for EmptyAttributeOwner_Impl {
    fn GetACPFromPoint(&self, _: *const POINT, _: u32) -> Result<i32> {
        Err(E_NOTIMPL.into())
    }
    fn GetTextExt(&self, _: i32, _: i32, _: *mut RECT, _: *mut BOOL) -> Result<()> {
        Err(TS_E_NOLAYOUT.into())
    }
    fn GetScreenExt(&self) -> Result<RECT> {
        Ok(RECT::default())
    }
    fn GetStatus(&self) -> Result<TS_STATUS> {
        Ok(TS_STATUS::default())
    }
    fn GetWnd(&self) -> Result<HWND> {
        Ok(self.window)
    }
    fn GetAttribute(&self, id: *const GUID) -> Result<VARIANT> {
        if unsafe { id.as_ref() } == Some(&GUID_PROP_INPUTSCOPE) {
            self.asked_for_scope.set(true);
        }
        Ok(VARIANT::default())
    }
}

impl ITfContextOwnerCompositionSink_Impl for EmptyAttributeOwner_Impl {
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

#[test]
fn field_with_empty_context_owner_attributes_converts() -> Result<()> {
    with_host(|host| unsafe {
        let manager: ITfThreadMgr =
            CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
        let documents = manager.CreateDocumentMgr()?;
        let owner = ComObject::new(EmptyAttributeOwner {
            window: GetFocus(),
            asked_for_scope: Cell::new(false),
        });
        let sink: ITfContextOwnerCompositionSink = owner.to_interface();
        let mut context = None;
        let mut cookie = 0;
        documents.CreateContext(host.owner_id, 0, &sink, &mut context, &mut cookie)?;
        let context = context.ok_or_else(failure)?;
        let source = context.cast::<ITfSource>()?;
        let advised = source.AdviseSink(
            &ITfContextOwner::IID,
            &owner.to_interface::<ITfContextOwner>(),
        )?;
        documents.Push(&context)?;
        let result = check_connected_commit(&host.tip, &context);
        source.UnadviseSink(advised)?;
        documents.Pop(TF_POPF_ALL)?;
        assert!(owner.asked_for_scope.get());
        result
    })
}

fn dispatch_pending() {
    unsafe {
        let mut message = MSG::default();
        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

fn cuas_context(manager: &ITfThreadMgr, edit: HWND) -> Result<Option<ITfContext>> {
    // On an inactive desktop the manager can have a CUAS document even when
    // GetFocus returns null. Match its active view to the actual EDIT.
    unsafe {
        let documents = manager.EnumDocumentMgrs()?;
        let mut slot = [None];
        let mut count = 0;
        loop {
            documents.Next(&mut slot, &mut count)?;
            let Some(document) = slot[0].take() else {
                return Ok(None);
            };
            if let Ok(context) = document.GetTop()
                && context.GetActiveView().and_then(|view| view.GetWnd()) == Ok(edit)
            {
                return Ok(Some(context));
            }
        }
    }
}

fn check_cuas_edit() -> Result<()> {
    unsafe {
        // Let IMM32 create the TSF document for the EDIT through CUAS.
        let edit = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            windows::core::w!("EDIT"),
            None,
            WS_POPUP | WS_VISIBLE,
            0,
            0,
            160,
            40,
            None,
            None,
            None,
            None,
        )
        .map_err(|e| {
            Error::new(
                e.code(),
                "could not create the Win32 EDIT for the CUAS test",
            )
        })?;
        let result = (|| {
            let _ = SetFocus(Some(edit));
            assert!(GetFocus() == edit, "Win32 EDIT could not acquire focus");
            let manager: ITfThreadMgr =
                CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
            let start = std::time::Instant::now();
            let context = loop {
                dispatch_pending();
                if let Some(context) = cuas_context(&manager, edit)? {
                    break context;
                }
                assert!(
                    start.elapsed() < Duration::from_secs(3),
                    "CUAS did not create a TSF context for the Win32 EDIT"
                );
                std::thread::sleep(Duration::from_millis(10));
            };
            SetKeyboardState(&[0; 256])?;
            check_connected_commit(&test_tip(&manager)?, &context)?;
            let start = std::time::Instant::now();
            loop {
                dispatch_pending();
                let mut text = [0; 3];
                if GetWindowTextW(edit, &mut text) == 1 && text[0] == '字' as u16 {
                    break;
                }
                assert!(
                    start.elapsed() < Duration::from_secs(1),
                    "EDIT did not receive the commit"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(())
        })();
        DestroyWindow(edit)?;
        result
    }
}

#[test]
fn ordinary_win32_edit_converts_through_cuas_on_a_hidden_desktop() -> Result<()> {
    use std::io::Write;
    use windows::Win32::System::StationsAndDesktops::*;
    if std::env::var_os("KANADE_TEST_SKIP_CUAS_EDIT").as_deref() == Some("1".as_ref()) {
        // Write directly so the reason is visible with libtest's default capture.
        writeln!(
            std::io::stderr().lock(),
            "Skipping Win32 EDIT conversion: KANADE_TEST_SKIP_CUAS_EDIT=1"
        )?;
        return Ok(());
    }
    let name = windows::core::HSTRING::from(format!("KanadeTipTest-{}", std::process::id()));
    let desktop = unsafe {
        CreateDesktopW(
            &name,
            None,
            None,
            DESKTOP_CONTROL_FLAGS(0),
            GENERIC_ALL.0,
            None,
        )
    }
    .map_err(|e| {
        Error::new(
            e.code(),
            "could not create the hidden desktop for the CUAS test",
        )
    })?;
    // The desktop handle stays open until the window-owning thread exits.
    let handle = desktop.0 as usize;
    let result = std::thread::spawn(move || unsafe {
        SetThreadDesktop(HDESK(handle as _)).map_err(|e| {
            Error::new(
                e.code(),
                "could not attach the CUAS test thread to its desktop",
            )
        })?;
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let result = check_cuas_edit();
        CoUninitialize();
        result
    })
    .join();
    unsafe { CloseDesktop(desktop)? };
    result.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
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
fn first_key_after_focus_connects_when_the_host_supplies_no_scope() -> Result<()> {
    with_host(|host| {
        host.store.supplies_scope.set(false);
        host.check_scope_value(Err(E_FAIL))?;
        let keys: ITfKeyEventSink = host.tip.to_interface();
        unsafe {
            keys.OnSetFocus(false)?;
            keys.OnSetFocus(true)?;
        }
        host.store.selection.set((0, 2));
        let server = serve(&[Reply::NormalCommit])?;
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

fn check_bypassed_field(configure: impl FnOnce(&Host) -> Result<()>) -> Result<()> {
    with_host(|host| {
        let server = serve(&[])?;
        assert!(host.tip.shared.connect(false) == Connection::Attached);
        configure(host)?;
        let selection = host.store.selection.get();
        let length = host.store.end();
        let vk = starter()?;
        host.store.preceding_reads.set(0);
        assert!(!host.test(vk)?);
        assert!(!host.down(vk)?);
        host.check_up(vk, false)?;
        host.tip.shared.client.borrow_mut().take();
        finish(server)?;

        // A disconnected TIP and a Down without Test use the same field policy.
        assert!(!host.test(vk)?);
        assert!(!host.down(vk)?);
        host.check_up(vk, false)?;
        assert!(!host.down(vk)?);
        assert_eq!(host.store.preceding_reads.get(), 0);
        assert_eq!(host.store.selection.get(), selection);
        assert_eq!(host.store.end(), length);
        assert_eq!(HINTS.get(), 0);
        assert_eq!(START_PROBES.get(), 0);
        Ok(())
    })
}

#[test]
fn field_with_unimplemented_attributes_converts() -> Result<()> {
    with_host(|host| unsafe {
        host.store.implements_attrs.set(false);
        for id in [TSATTRID_Text_ReadOnly, GUID_PROP_INPUTSCOPE] {
            assert_eq!(
                host.context.GetAppProperty(&id).err().map(|e| e.code()),
                Some(E_NOTIMPL)
            );
        }
        check_connected_commit(&host.tip, &host.context)?;
        assert!(*host.store.text.borrow() == [65, 66, '字' as u16]);
        Ok(())
    })
}

#[test]
fn field_with_unavailable_attribute_value_converts() -> Result<()> {
    for (id, error) in [
        (GUID_PROP_INPUTSCOPE, E_FAIL),
        (GUID_PROP_INPUTSCOPE, E_NOTIMPL),
        (TSATTRID_Text_ReadOnly, E_NOTIMPL),
    ] {
        with_host(|host| unsafe {
            host.store.range_readonly.set(Some(false));
            host.store.value_error.set(Some((id, error)));
            let start = host.context.GetStart(host.owner_cookie)?;
            let property = host.context.GetAppProperty(&id)?;
            // TSF can translate the store's failure to E_FAIL.
            assert!(property.GetValue(host.owner_cookie, &start).is_err());
            check_connected_commit(&host.tip, &host.context)?;
            assert!(*host.store.text.borrow() == [65, 66, '字' as u16]);
            Ok(())
        })?;
    }
    Ok(())
}

#[test]
fn an_unavailable_readonly_attribute_still_respects_password_scope() -> Result<()> {
    check_bypassed_field(|host| {
        host.store.range_readonly.set(Some(false));
        host.store
            .value_error
            .set(Some((TSATTRID_Text_ReadOnly, E_NOTIMPL)));
        host.store.set_scope(IS_PASSWORD);
        Ok(())
    })
}

#[test]
fn field_with_empty_scope_list_converts() -> Result<()> {
    with_host(|host| {
        host.store.scopes.borrow_mut().clear();
        check_connected_commit(&host.tip, &host.context)?;
        assert!(*host.store.text.borrow() == [65, 66, '字' as u16]);
        Ok(())
    })
}

#[test]
fn field_with_long_scope_list_checks_the_first_256_entries() -> Result<()> {
    let mut kinds = [IS_DEFAULT; 257];
    kinds[256] = IS_PASSWORD;
    with_host(|host| {
        host.store.scopes.replace(kinds.to_vec());
        check_connected_commit(&host.tip, &host.context)?;
        assert!(*host.store.text.borrow() == [65, 66, '字' as u16]);
        Ok(())
    })?;

    kinds[255] = IS_PRIVATE;
    with_host(|host| {
        host.store.scopes.replace(kinds.to_vec());
        let server = serve(&[Reply::PrivateCommit])?;
        let vk = starter()?;
        assert!(host.test(vk)? && host.down(vk)?);
        host.tip.shared.client.borrow_mut().take();
        finish(server)
    })?;

    kinds[0] = IS_PRIVATE;
    kinds[255] = IS_NUMERIC_PIN;
    check_bypassed_field(|host| {
        host.store.scopes.replace(kinds.to_vec());
        Ok(())
    })
}

#[test]
fn field_scope_list_failure_bypasses_conversion() -> Result<()> {
    check_bypassed_field(|host| {
        host.store.set_scope(IS_PASSWORD);
        host.store.scope_fails.set(true);
        Ok(())
    })
}

#[test]
fn field_with_mixed_password_and_default_scopes_bypasses_conversion() -> Result<()> {
    check_bypassed_field(|host| {
        host.store.text.borrow_mut().push(67);
        host.store.mixed_scope.set(true);
        host.store.selection.set((1, 3));
        Ok(())
    })
}

#[test]
fn field_with_empty_scope_value_converts() -> Result<()> {
    with_host(|host| {
        host.store.empty_scope.set(true);
        host.check_scope_value(Ok(VT_EMPTY))?;
        host.store.selection.set((1, 2));
        check_connected_commit(&host.tip, &host.context)?;
        assert!(*host.store.text.borrow() == [65, '字' as u16]);
        Ok(())
    })
}

#[test]
fn field_without_transition_support_converts() -> Result<()> {
    with_host(|host| {
        host.store.supports_transitions.set(false);
        host.store.selection.set((1, 2));
        check_connected_commit(&host.tip, &host.context)?;
        assert!(*host.store.text.borrow() == [65, '字' as u16]);
        Ok(())
    })
}

#[test]
fn field_private_at_test_stays_private_for_down() -> Result<()> {
    with_host(|host| {
        host.store.set_scope(IS_PRIVATE);
        let server = serve(&[Reply::PrivateCommit])?;
        let vk = starter()?;
        assert!(host.test(vk)?);
        host.store.set_scope(IS_DEFAULT);
        assert!(host.down(vk)?);
        host.tip.shared.client.borrow_mut().take();
        finish(server)
    })
}

#[test]
fn field_with_readonly_range_bypasses_conversion() -> Result<()> {
    for selection in [(2, 2), (1, 3)] {
        check_bypassed_field(|host| {
            host.store.text.borrow_mut().push(67);
            host.store.selection.set(selection);
            host.store.range_readonly.set(Some(true));
            Ok(())
        })?;
    }
    Ok(())
}

#[test]
fn field_with_uniform_writable_selection_converts() -> Result<()> {
    with_host(|host| {
        host.store.range_readonly.set(Some(false));
        host.store.selection.set((1, 2));
        let server = serve(&[Reply::NormalCommit])?;
        let vk = starter()?;
        assert!(host.test(vk)? && host.down(vk)?);
        host.tip.shared.client.borrow_mut().take();
        finish(server)?;
        assert!(*host.store.text.borrow() == [65, '字' as u16]);
        Ok(())
    })
}

#[test]
fn field_with_pin_or_password_scope_bypasses_conversion() -> Result<()> {
    for scope in [
        IS_NUMERIC_PASSWORD,
        IS_NUMERIC_PIN,
        IS_ALPHANUMERIC_PIN,
        IS_ALPHANUMERIC_PIN_SET,
        IS_PASSWORD,
    ] {
        check_bypassed_field(|host| {
            host.store.set_scope(scope);
            Ok(())
        })?;
    }
    Ok(())
}

#[test]
fn field_with_disabled_keyboard_bypasses_conversion() -> Result<()> {
    check_bypassed_field(|host| {
        host.store.set_scope(IS_PRIVATE);
        let compartments = host.context.cast::<ITfCompartmentMgr>()?;
        unsafe {
            compartments
                .GetCompartment(&GUID_COMPARTMENT_KEYBOARD_DISABLED)?
                .SetValue(host.owner_id, &VARIANT::from(1i32))
        }
    })
}

#[test]
fn field_with_private_scope_converts_without_reading_context() -> Result<()> {
    with_host(|host| {
        let server = serve(&[Reply::NormalCommit, Reply::PrivateCommit])?;
        assert!(host.tip.shared.connect(false) == Connection::Attached);
        let vk = starter()?;
        assert!(host.test(vk)? && host.down(vk)?);
        host.check_up(vk, true)?;
        assert!(host.store.preceding_reads.get() > 0);
        host.store.set_scope(IS_PRIVATE);
        host.store.preceding_reads.set(0);
        assert!(host.test(vk)?);
        assert_eq!(host.store.preceding_reads.get(), 0);
        assert!(host.down(vk)?);
        host.check_up(vk, true)?;
        host.tip.shared.client.borrow_mut().take();
        finish(server)?;
        assert_eq!(host.store.preceding_reads.get(), 0);
        assert_eq!(host.store.end(), 4);
        Ok(())
    })
}

#[test]
fn field_passed_in_test_stays_passed_if_it_becomes_writable() -> Result<()> {
    with_host(|host| {
        host.store.set_scope(IS_PASSWORD);
        let vk = starter()?;
        assert!(!host.test(vk)?);
        host.store.set_scope(IS_DEFAULT);
        assert!(!host.test(vk)?);
        assert!(!host.down(vk)?);
        host.check_up(vk, false)?;
        assert!(host.test(vk)? && host.down(vk)?);
        Ok(())
    })
}

#[test]
fn outage_uses_the_connected_starter_policy_in_writable_fields() -> Result<()> {
    for scope in [None, Some(IS_DEFAULT), Some(IS_PRIVATE)] {
        with_host(|host| {
            if scope.is_none() {
                host.store.supplies_scope.set(false);
            }
            host.store.set_scope(scope.unwrap_or(IS_DEFAULT));
            host.store.range_readonly.set(Some(false));
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
            assert!(host.down(vk)?);
            host.check_up(vk, true)?;
            finish(server)?;

            assert!(host.test(vk)? && host.down(vk)?);
            host.check_up(vk, true)?;
            assert!(host.down(vk)?);
            host.check_up(vk, true)?;
            assert!(!host.test(8)?);
            host.check_up(8, false)?;
            assert_eq!(host.store.end(), 3);
            assert_eq!(HINTS.get(), 1);
            assert_eq!(START_PROBES.get(), 3);
            Ok(())
        })?;
    }
    Ok(())
}
