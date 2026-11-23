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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Refusal {
    WriteLock,
    Start,
    SetText,
    SetSelection,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DuringEdit {
    Start,
    SetText,
    AfterReadLock,
    AfterWriteLock,
}

type Callback = Box<dyn FnOnce() -> Result<()>>;

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
    refusal: Cell<Option<Refusal>>,
    callback: Cell<Option<(DuringEdit, Callback)>>,
    refuse_nested_lock: Cell<bool>,
    compositions: Cell<u32>,
    ends: Cell<u32>,
    no_layout: Cell<bool>,
    busy: Cell<bool>,
    readonly: Cell<bool>,
}

impl Store {
    fn interrupt(&self, point: DuringEdit) -> Result<()> {
        if let Some((at, callback)) = self.callback.take() {
            if at == point {
                callback()?;
            } else {
                self.callback.set(Some((at, callback)));
            }
        }
        Ok(())
    }
    fn refuses(&self, operation: Refusal) -> bool {
        if self.refusal.get() == Some(operation) {
            self.refusal.set(None);
            true
        } else {
            false
        }
    }
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
        if self.refuses(Refusal::Start) {
            return Ok(false.into());
        }
        self.compositions.set(self.compositions.get() + 1);
        self.interrupt(DuringEdit::Start)?;
        Ok(true.into())
    }
    fn OnUpdateComposition(&self, _: Ref<ITfCompositionView>, _: Ref<ITfRange>) -> Result<()> {
        Ok(())
    }
    fn OnEndComposition(&self, _: Ref<ITfCompositionView>) -> Result<()> {
        self.compositions.set(self.compositions.get() - 1);
        self.ends.set(self.ends.get() + 1);
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
        if flags & TS_LF_READWRITE.0 == TS_LF_READWRITE.0 && self.refuses(Refusal::WriteLock) {
            return Ok(TS_E_SYNCHRONOUS);
        }
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
        if flags & TS_LF_READWRITE.0 == TS_LF_READWRITE.0 {
            self.busy.set(self.refuse_nested_lock.get());
            let callback = self.interrupt(DuringEdit::AfterWriteLock);
            self.busy.set(false);
            callback?;
        } else {
            self.interrupt(DuringEdit::AfterReadLock)?;
        }
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
        if self.refuses(Refusal::SetSelection) {
            return Err(failure());
        }
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
        if self.refuses(Refusal::SetText) {
            return Err(failure());
        }
        let change = self.replace(start, end, units(text, len))?;
        self.interrupt(DuringEdit::SetText)?;
        Ok(change)
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
    static FAIL_ACK: Cell<bool> = const { Cell::new(false) };
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
        if FAIL_ACK.replace(false) {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
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
    fn composition_ranges(&self) -> Result<Vec<(i32, i32)>> {
        unsafe {
            let views = self
                .context
                .cast::<ITfContextComposition>()?
                .EnumCompositions()?;
            let mut ranges = Vec::new();
            loop {
                let mut slot = [None];
                let mut fetched = 0;
                views.Next(&mut slot, &mut fetched)?;
                let Some(view) = slot[0].take() else {
                    return Ok(ranges);
                };
                let range: ITfRangeACP = view.GetRange()?.cast()?;
                let (mut start, mut length) = (0, 0);
                range.GetExtent(&mut start, &mut length)?;
                ranges.push((start, length));
            }
        }
    }

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
        set_held(vk, true)?;
        let keys: ITfKeyEventSink = self.tip.to_interface();
        Ok(unsafe { keys.OnTestKeyDown(&self.context, WPARAM(vk), LPARAM(1))? }.as_bool())
    }

    fn down(&self, vk: usize) -> Result<bool> {
        set_held(vk, true)?;
        let keys: ITfKeyEventSink = self.tip.to_interface();
        Ok(unsafe { keys.OnKeyDown(&self.context, WPARAM(vk), LPARAM(1))? }.as_bool())
    }

    fn check_up(&self, vk: usize, taken: bool) -> Result<()> {
        KeySource::Wpf.key(vk, false).check_up(self, taken)?;
        let keys: ITfKeyEventSink = self.tip.to_interface();
        unsafe {
            assert!(
                !keys
                    .OnTestKeyUp(&self.context, WPARAM(vk), LPARAM(0xc000_0001u32 as isize))?
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
    tip.shared.japanese.set(true);
    Ok(ComObject::new(tip))
}

fn test_document(
    manager: &ITfThreadMgr,
    id: u32,
    tip: ComObject<Tip>,
) -> Result<(ITfDocumentMgr, Host)> {
    unsafe {
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
            refusal: Cell::new(None),
            callback: Cell::new(None),
            refuse_nested_lock: Cell::new(false),
            compositions: Cell::new(0),
            ends: Cell::new(0),
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
            tip,
            owner_id: id,
            owner_cookie: cookie,
            context,
            store,
        };
        Ok((documents, host))
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
        let (documents, host) = test_document(&manager, id, test_tip(&manager)?)?;
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
            translate(WPARAM(vk), LPARAM(1))
                .is_some_and(|k| !k.text.is_empty() && k.text.is_ascii())
        })
        .ok_or_else(failure)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KeySource {
    Wpf,
    Hook,
}

#[derive(Clone, Copy)]
struct HostKey {
    vk: usize,
    flags: u32,
    source: KeySource,
}

impl KeySource {
    fn key(self, vk: usize, extended: bool) -> HostKey {
        let (vk, flags) = match self {
            Self::Wpf if vk == usize::from(VK_LSHIFT.0) => (usize::from(VK_SHIFT.0), 0x002a_0001),
            Self::Wpf if vk == usize::from(VK_RSHIFT.0) => (usize::from(VK_SHIFT.0), 0x0036_0001),
            Self::Wpf => (vk, 1),
            Self::Hook => {
                let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) };
                let vk = match VIRTUAL_KEY(vk as u16) {
                    VK_LCONTROL | VK_RCONTROL => usize::from(VK_CONTROL.0),
                    _ => vk,
                };
                (vk, 1 | ((scan & 0xff) << 16) | (u32::from(extended) << 24))
            }
        };
        HostKey {
            vk,
            flags,
            source: self,
        }
    }
}

fn set_held(vk: usize, down: bool) -> Result<()> {
    let mut state = [0; 256];
    unsafe {
        GetKeyboardState(&mut state)?;
        *state.get_mut(vk).ok_or_else(failure)? = if down { 0x80 } else { 0 };
        if matches!(VIRTUAL_KEY(vk as u16), VK_LCONTROL | VK_RCONTROL) {
            let left = state.get(usize::from(VK_LCONTROL.0)).ok_or_else(failure)?;
            let right = state.get(usize::from(VK_RCONTROL.0)).ok_or_else(failure)?;
            *state
                .get_mut(usize::from(VK_CONTROL.0))
                .ok_or_else(failure)? = left | right;
        }
        SetKeyboardState(&state)
    }
}

impl HostKey {
    fn down(self, host: &Host, repeat: bool) -> Result<bool> {
        self.deliver(&host.tip.to_interface(), &host.context, repeat)
    }

    fn deliver(self, keys: &ITfKeyEventSink, context: &ITfContext, repeat: bool) -> Result<bool> {
        set_held(self.vk, true)?;
        let flags = self.flags | (u32::from(repeat && self.source == KeySource::Hook) << 30);
        let l = LPARAM(flags as isize);
        unsafe {
            Ok(keys.OnTestKeyDown(context, WPARAM(self.vk), l)?.as_bool()
                && keys.OnKeyDown(context, WPARAM(self.vk), l)?.as_bool())
        }
    }

    fn check_up(self, host: &Host, taken: bool) -> Result<()> {
        set_held(self.vk, false)?;
        let keys: ITfKeyEventSink = host.tip.to_interface();
        let l = LPARAM((self.flags | 0xc000_0000) as isize);
        unsafe {
            assert_eq!(
                keys.OnTestKeyUp(&host.context, WPARAM(self.vk), l)?
                    .as_bool(),
                taken
            );
            if taken {
                assert!(keys.OnKeyUp(&host.context, WPARAM(self.vk), l)?.as_bool());
            }
        }
        Ok(())
    }
}

#[test]
fn key_routing_repeat_after_composition_ends() -> Result<()> {
    for source in [KeySource::Wpf, KeySource::Hook] {
        for (vk, replies) in [
            (8, &[Reply::Compose, Reply::Cancel, Reply::PassBackspace]),
            (0x0d, &[Reply::Compose, Reply::Commit, Reply::Pass]),
        ] {
            with_host(|host| {
                let server = serve(replies)?;
                press(&host.tip, &host.context, starter()?)?;
                let key = source.key(vk, false);
                assert!(key.down(host, false)?);
                assert!(!key.down(host, true)?);
                key.check_up(host, false)?;
                host.tip.shared.client.borrow_mut().take();
                finish(server)
            })?;
        }
    }
    Ok(())
}

#[test]
fn key_routing_ctrl_while_a_character_is_held() -> Result<()> {
    for source in [KeySource::Wpf, KeySource::Hook] {
        for side in [VK_LCONTROL, VK_RCONTROL] {
            with_host(|host| {
                let server = serve(&[Reply::Compose, Reply::Pass])?;
                let letter = source.key(starter()?, false);
                let ctrl = source.key(usize::from(side.0), side == VK_RCONTROL);
                assert!(letter.down(host, false)?);
                assert!(!ctrl.down(host, false)?);
                assert!(!letter.down(host, true)?);
                ctrl.check_up(host, false)?;
                letter.check_up(host, false)?;
                host.tip.shared.client.borrow_mut().take();
                finish(server)
            })?;
        }
    }
    Ok(())
}

#[test]
fn key_routing_held_across_mode_changes() -> Result<()> {
    for source in [KeySource::Wpf, KeySource::Hook] {
        with_host(|host| unsafe {
            let manager: ITfThreadMgr =
                CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
            let tid = host.tip.shared.tid.get();
            let mode = input_mode::Mode::attach(&manager, tid, &host.tip.to_interface())?;
            let switch = mode.switch();
            let original = switch.japanese();
            host.tip.mode.replace(Some(mode));
            let key = source.key(starter()?, false);
            for to_japanese in [true, false] {
                use input_mode::Command::{Off, On};
                switch.apply(tid, if to_japanese { Off } else { On })?;
                let server = serve(&[Reply::Commit])?;
                assert_eq!(key.down(host, false)?, !to_japanese);
                switch.apply(tid, if to_japanese { On } else { Off })?;
                assert_eq!(key.down(host, true)?, to_japanese);
                key.check_up(host, false)?;
                host.tip.shared.client.borrow_mut().take();
                finish(server)?;
            }
            switch.apply(
                tid,
                if original {
                    input_mode::Command::On
                } else {
                    input_mode::Command::Off
                },
            )
        })?;
    }
    Ok(())
}

#[test]
fn key_routing_overlapping_return_keys() -> Result<()> {
    for source in [KeySource::Wpf, KeySource::Hook] {
        for (pad_up_first, intervening_down) in [(true, false), (false, false), (true, true)] {
            with_host(|host| {
                let server = serve(&[Reply::Compose, Reply::Commit, Reply::Pass])?;
                press(&host.tip, &host.context, starter()?)?;
                let main = source.key(0x0d, false);
                let pad = source.key(0x0d, true);
                assert!(main.down(host, false)?);
                assert!(!pad.down(host, false)?);
                if pad_up_first {
                    pad.check_up(host, false)?;
                }
                if intervening_down {
                    let ctrl = source.key(usize::from(VK_LCONTROL.0), false);
                    assert!(!ctrl.down(host, false)?);
                    ctrl.check_up(host, false)?;
                }
                main.check_up(host, source == KeySource::Hook && !intervening_down)?;
                if !pad_up_first {
                    pad.check_up(host, false)?;
                }
                host.tip.shared.client.borrow_mut().take();
                finish(server)
            })?;
        }
    }
    Ok(())
}

#[test]
fn key_routing_focus_and_sink_detachment() -> Result<()> {
    for source in [KeySource::Wpf, KeySource::Hook] {
        with_host(|host| unsafe {
            let key = source.key(starter()?, false);
            let keys: ITfKeyEventSink = host.tip.to_interface();
            let server = serve(&[Reply::Commit])?;
            assert!(key.down(host, false)?);
            keys.OnSetFocus(false)?;
            keys.OnSetFocus(true)?;
            key.check_up(host, true)?;
            finish(server)?;
            let server = serve(&[Reply::Commit])?;
            assert!(key.down(host, false)?);
            host.tip
                .to_interface::<ITfTextInputProcessorEx>()
                .Deactivate()?;
            key.check_up(host, false)?;
            finish(server)
        })?;
    }
    Ok(())
}

#[test]
fn key_routing_new_press_after_missed_release() -> Result<()> {
    for source in [KeySource::Wpf, KeySource::Hook] {
        with_host(|host| {
            let key = source.key(starter()?, false);
            let server = serve(&[Reply::Decline, Reply::Commit])?;
            assert!(!key.down(host, false)?);
            set_held(key.vk, false)?;
            if source == KeySource::Wpf {
                // Another Down observes the release that the sink missed.
                let ctrl = source.key(usize::from(VK_LCONTROL.0), false);
                assert!(!ctrl.down(host, false)?);
                ctrl.check_up(host, false)?;
            }
            assert!(key.down(host, false)?);
            key.check_up(host, true)?;
            key.check_up(host, false)?;
            host.tip.shared.client.borrow_mut().take();
            finish(server)
        })?;
    }
    Ok(())
}

#[test]
fn key_routing_wpf_passes_standalone_shift_downs_and_up() -> Result<()> {
    with_host(|host| unsafe {
        let keys: ITfKeyEventSink = host.tip.to_interface();
        for side in [VK_LSHIFT, VK_RSHIFT] {
            let shift = KeySource::Wpf.key(usize::from(side.0), false);
            let w = WPARAM(shift.vk);
            let l = LPARAM(shift.flags as isize);
            set_held(shift.vk, true)?;
            // WPF supplies the same flags for the first press and the repeat.
            for _ in 0..2 {
                assert!(!keys.OnTestKeyDown(&host.context, w, l)?.as_bool());
                assert!(!keys.OnKeyDown(&host.context, w, l)?.as_bool());
            }
            assert!(!keys.OnKeyDown(&host.context, w, l)?.as_bool());
            set_held(shift.vk, false)?;
            let up = LPARAM((shift.flags | 0xc000_0000) as isize);
            assert!(!keys.OnTestKeyUp(&host.context, w, up)?.as_bool());
            assert!(!keys.OnKeyUp(&host.context, w, up)?.as_bool());
        }
        Ok(())
    })
}

#[test]
fn key_routing_engine_decline_then_consumed_repeat() -> Result<()> {
    for source in [KeySource::Wpf, KeySource::Hook] {
        with_host(|host| unsafe {
            let server = serve(&[Reply::Decline, Reply::Commit])?;
            let key = source.key(starter()?, false);
            let sink: ITfKeyEventSink = host.tip.to_interface();
            let l = LPARAM(key.flags as isize);
            set_held(key.vk, true)?;
            assert!(
                sink.OnTestKeyDown(&host.context, WPARAM(key.vk), l)?
                    .as_bool()
            );
            assert!(!sink.OnKeyDown(&host.context, WPARAM(key.vk), l)?.as_bool());
            assert!(key.down(host, true)?);
            key.check_up(host, false)?;
            host.tip.shared.client.borrow_mut().take();
            finish(server)
        })?;
    }
    Ok(())
}

#[test]
fn key_routing_prediction_uses_the_keyboard_layout() -> Result<()> {
    with_host(|host| {
        let server = serve(&[])?;
        set_held(usize::from(VK_SHIFT.0), true)?;
        // Shift+0 produces no character on JIS. Use a reserved VK on layouts
        // where it does produce a character.
        let vk = [0x30, 0x3a]
            .into_iter()
            .find(|&vk| translate(WPARAM(vk), LPARAM(1)).is_none())
            .ok_or_else(failure)?;
        assert!(!host.test(vk)?);
        host.check_up(vk, false)?;
        host.tip.shared.client.borrow_mut().take();
        finish(server)
    })
}

#[test]
fn key_routing_denied_read_ends_the_mark_and_passes_composition_commands() -> Result<()> {
    check_denied_read_during_composition(false)
}

#[test]
fn key_routing_denied_read_defers_the_mark_end_after_a_refused_write() -> Result<()> {
    check_denied_read_during_composition(true)
}

fn check_denied_read_during_composition(refuse_write: bool) -> Result<()> {
    for deny_at_test in [true, false] {
        for vk in [8, 0x0d, 0x1b, 0x20] {
            with_host(|host| {
                let server = serve(&[Reply::Converted])?;
                press(&host.tip, &host.context, starter()?)?;
                let displayed = host.store.text.borrow().clone();
                assert_eq!(host.store.compositions.get(), 1);
                if !deny_at_test {
                    assert!(host.test(vk)?);
                }
                host.store.reject_lock_after.set(Some(0));
                if refuse_write {
                    host.store.refusal.set(Some(Refusal::WriteLock));
                }
                if deny_at_test {
                    assert!(!host.test(vk)?);
                } else {
                    assert!(!host.down(vk)?);
                }
                if refuse_write {
                    dispatch_pending();
                }
                assert_eq!(host.store.compositions.get(), 0);
                assert_eq!(host.store.ends.get(), 1);
                assert!(host.store.text.borrow().as_slice() == displayed);
                host.check_up(vk, false)?;
                finish(server)
            })?;
        }
    }
    Ok(())
}

#[test]
fn key_routing_down_after_focus_moves_to_a_password_field() -> Result<()> {
    with_host(|host| unsafe {
        let vk = starter()?;
        let server = serve(&[])?;
        assert!(host.test(vk)?);
        let keys: ITfKeyEventSink = host.tip.to_interface();
        keys.OnSetFocus(false)?;
        host.store.set_scope(IS_PASSWORD);
        keys.OnSetFocus(true)?;
        assert!(!host.down(vk)?);
        host.check_up(vk, false)?;
        finish(server)
    })
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
                3 => host.tip.shared.japanese.set(false),
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
fn key_routing_down_passes_a_claim_in_a_newly_protected_field() -> Result<()> {
    for connected in [false, true] {
        with_host(|host| {
            let vk = starter()?;
            let server = connected.then(|| serve(&[])).transpose()?;
            assert!(host.test(vk)?);
            let opens = OPENS.get();
            host.store.set_scope(IS_PASSWORD);
            assert!(!host.down(vk)?);
            host.check_up(vk, false)?;
            assert_eq!(host.store.preceding_reads.get(), 0);
            assert_eq!(OPENS.get(), opens);
            assert_eq!(START_PROBES.get(), 0);
            assert_eq!(HINTS.get(), 0);
            if let Some(server) = server {
                finish(server)?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

#[test]
fn key_routing_down_ends_the_displayed_mark_in_a_newly_protected_field() -> Result<()> {
    with_host(|host| {
        let server = serve(&[Reply::Converted])?;
        press(&host.tip, &host.context, starter()?)?;
        let displayed = host.store.text.borrow().clone();
        assert_eq!(host.store.compositions.get(), 1);
        assert!(host.test(0x0d)?);
        host.store.set_scope(IS_PASSWORD);
        assert!(!host.down(0x0d)?);
        assert_eq!(host.store.compositions.get(), 0);
        assert_eq!(host.store.ends.get(), 1);
        assert!(host.store.text.borrow().as_slice() == displayed);
        host.check_up(0x0d, false)?;
        finish(server)
    })
}

#[test]
fn key_routing_down_passes_a_field_protected_during_reconnection() -> Result<()> {
    with_host(|host| {
        let vk = starter()?;
        assert!(host.test(vk)?);
        let server = serve(&[])?;
        let store = host.store.clone();
        host.store.callback.set(Some((
            DuringEdit::AfterReadLock,
            Box::new(move || {
                store.set_scope(IS_PASSWORD);
                Ok(())
            }),
        )));
        assert!(!host.down(vk)?);
        host.check_up(vk, false)?;
        assert_eq!(HELLOS.get(), 1);
        assert_eq!(host.store.preceding_reads.get(), 0);
        assert_eq!(HINTS.get(), 0);
        finish(server)
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reply {
    Candidates,
    Select,
    Poll,
    Compose,
    CommitAndCompose,
    Restore,
    UnackedCompose,
    ConvertedWithoutAck,
    Converted,
    CommitConverted,
    Cancel,
    Commit,
    PrivateCommit,
    PrivateCompose,
    NormalCommit,
    Pass,
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
    serve_edits(steps, None)
}

fn serve_edits(
    steps: &'static [Reply],
    rejected: Option<u64>,
) -> Result<std::thread::JoinHandle<std::io::Result<()>>> {
    serve_keys(steps, rejected, &[])
}

fn serve_keys(
    steps: &'static [Reply],
    rejected: Option<u64>,
    expected: &'static [SpecialKey],
) -> Result<std::thread::JoinHandle<std::io::Result<()>>> {
    let mut listener = listener()?;
    Ok(std::thread::spawn(move || {
        let pipe = accept_session(&mut listener)?;
        drop(listener);
        reply_to_session(pipe, steps.iter().copied(), rejected, expected)
    }))
}

fn accept_session(listener: &mut Listener) -> std::io::Result<Pipe> {
    listener
        .accept()
        .map_err(|_| std::io::Error::other("accept failed"))
}

fn reply_to_session(
    mut pipe: Pipe,
    steps: impl IntoIterator<Item = Reply>,
    rejected: Option<u64>,
    expected: &[SpecialKey],
) -> std::io::Result<()> {
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
    let mut seq = 0;
    for (i, step) in steps.into_iter().enumerate() {
        pipe.begin(ipc::KEY_TIMEOUT);
        let request: ipc::Request = ipc::receive(&mut pipe)?;
        if step == Reply::Poll {
            let Some(ipc::request::Body::Poll(poll)) = request.body else {
                return Err(std::io::Error::other("expected candidate poll"));
            };
            assert_eq!(poll.candidate_revision, 1);
            ipc::send(
                &mut pipe,
                &ipc::Response {
                    body: Some(ipc::response::Body::Candidates(ipc::CandidateUpdate {
                        revision: poll.revision,
                        pending: true,
                        ..Default::default()
                    })),
                },
            )?;
            continue;
        }
        seq += 1;
        let key = match request.body {
            Some(ipc::request::Body::Restore(r)) if step == Reply::Restore => {
                assert!(r.reading == "じ");
                ipc::KeyRequest {
                    event_seq: r.event_seq,
                    revision: r.revision,
                    ..Default::default()
                }
            }
            Some(ipc::request::Body::Select(r)) if step == Reply::Select => {
                assert!(r.submit && r.handle == Some(test_candidate()));
                ipc::KeyRequest {
                    event_seq: r.event_seq,
                    revision: r.revision,
                    ..Default::default()
                }
            }
            Some(ipc::request::Body::Key(key))
                if !matches!(step, Reply::Restore | Reply::Select) =>
            {
                key
            }
            _ => return Err(std::io::Error::other("unexpected session request")),
        };
        assert_eq!(key.event_seq, seq);
        if let Some(expected) = expected.get(i) {
            assert!(
                key.key
                    .as_ref()
                    .is_some_and(|k| k.special == *expected as i32)
            );
        }
        assert_eq!(
            key.passed_through,
            matches!(step, Reply::Pass | Reply::PassBackspace)
        );
        if matches!(
            step,
            Reply::PrivateCommit | Reply::PrivateCompose | Reply::NormalCommit
        ) {
            let observed = key
                .context
                .as_ref()
                .ok_or_else(|| std::io::Error::other("missing field context"))?;
            let private = step != Reply::NormalCommit;
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
        let composing = matches!(
            step,
            Reply::Compose
                | Reply::PrivateCompose
                | Reply::Converted
                | Reply::CommitAndCompose
                | Reply::Restore
                | Reply::UnackedCompose
                | Reply::ConvertedWithoutAck
                | Reply::Candidates
        );
        let (reading, surface, width) =
            if matches!(step, Reply::Converted | Reply::ConvertedWithoutAck) {
                ("かんじ", "漢字", 2)
            } else {
                ("じ", "じ", 1)
            };
        let commit = matches!(
            step,
            Reply::Commit
                | Reply::CommitConverted
                | Reply::Select
                | Reply::PrivateCommit
                | Reply::NormalCommit
                | Reply::CommitAndCompose
        );
        ipc::send(
            &mut pipe,
            &ipc::Response {
                body: Some(ipc::response::Body::Key(KeyReply {
                    revision: key.revision,
                    event_seq: key.event_seq,
                    consumed: commit || composing || step == Reply::Cancel,
                    preedit: composing.then(|| Preedit {
                        segments: vec![ipc::Segment {
                            reading: reading.into(),
                            value: surface.into(),
                            value_utf16: width,
                            ..Default::default()
                        }],
                        cursor_utf16: width,
                    }),
                    commit: commit.then(|| ipc::Commit {
                        value: if step == Reply::CommitConverted {
                            "漢字"
                        } else {
                            "字"
                        }
                        .into(),
                        ..Default::default()
                    }),
                    candidates: (step == Reply::Candidates).then(|| ipc::CandidatePage {
                        items: vec![ipc::CandidateItem {
                            handle: Some(test_candidate()),
                            number: 1,
                            ..Default::default()
                        }],
                        candidate_revision: 1,
                        total: 1,
                        refinement_pending: true,
                        ..Default::default()
                    }),
                    key_policy: Some(ipc::KeyPolicySummary {
                        enabled: true,
                        composing,
                        ..Default::default()
                    }),
                    ..Default::default()
                })),
            },
        )?;
        if matches!(step, Reply::UnackedCompose | Reply::ConvertedWithoutAck) {
            break;
        }
        let request: ipc::Request = ipc::receive(&mut pipe)?;
        let Some(ipc::request::Body::Ack(ack)) = request.body else {
            return Err(std::io::Error::other("expected edit acknowledgement"));
        };
        assert_eq!(ack.event_seq, key.event_seq);
        assert_eq!(ack.applied, rejected != Some(key.event_seq));
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
}

fn finish(server: std::thread::JoinHandle<std::io::Result<()>>) -> Result<()> {
    server.join().map_err(|_| failure())?.map_err(|_| failure())
}

// Start a TSF composition, then commit it. The peer checks both edit ACKs.
fn check_connected_commit(tip: &ComObject<Tip>, context: &ITfContext) -> Result<()> {
    let server = serve(&[Reply::Compose, Reply::Commit])?;
    assert!(tip.shared.connect(false) == Connection::Attached);
    for (vk, composing) in [(starter()?, true), (0x0d, false)] {
        press(tip, context, vk)?;
        assert_eq!(tip.shared.composition.borrow().is_some(), composing);
    }
    tip.shared.client.borrow_mut().take();
    finish(server)
}

fn press(tip: &ComObject<Tip>, context: &ITfContext, vk: usize) -> Result<()> {
    let keys: ITfKeyEventSink = tip.to_interface();
    set_held(vk, true)?;
    unsafe {
        assert!(
            keys.OnTestKeyDown(context, WPARAM(vk), LPARAM(1))?
                .as_bool()
        );
        assert!(keys.OnKeyDown(context, WPARAM(vk), LPARAM(1))?.as_bool());
        set_held(vk, false)?;
        assert!(
            keys.OnKeyUp(context, WPARAM(vk), LPARAM(0xc000_0001u32 as isize))?
                .as_bool()
        );
    }
    Ok(())
}

// These callbacks cross a COM boundary. Return their result to the test
// before asserting anything about key ownership or document contents.
fn deliver(keys: &ITfKeyEventSink, context: &ITfContext, vk: usize) -> Result<bool> {
    KeySource::Wpf.key(vk, false).deliver(keys, context, false)
}

fn compare_interrupted_keys(at: DuringEdit) -> Result<()> {
    let mut reference = None;
    for nested in [false, true] {
        with_host(|host| {
            let manager: ITfThreadMgr =
                unsafe { CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)? };
            unsafe { manager.SetFocus(&host.context.GetDocumentMgr()?)? };
            host.tip.shared.manager.replace(Some(manager));
            let server = serve_keys(
                &[
                    Reply::Converted,
                    Reply::Compose,
                    Reply::Commit,
                    Reply::Compose,
                ],
                None,
                &[
                    SpecialKey::None,
                    SpecialKey::Backspace,
                    SpecialKey::Enter,
                    SpecialKey::None,
                ],
            )?;
            let vk = starter()?;
            let keys = host.tip.to_interface();
            let context = host.context.clone();
            let accepted = Rc::new(Cell::new(false));
            let result = accepted.clone();
            let more = move || {
                let mut all = true;
                for key in [8, 0x0d, vk] {
                    all &= deliver(&keys, &context, key)?;
                }
                result.set(all);
                Ok(())
            };
            if nested {
                host.store.refuse_nested_lock.set(true);
                host.store.callback.set(Some((at, Box::new(more))));
                press(&host.tip, &host.context, vk)?;
            } else {
                press(&host.tip, &host.context, vk)?;
                more()?;
            }
            assert!(accepted.get());
            assert_eq!(HELLOS.get(), 1);
            assert_eq!(OPENS.get(), 1);
            let state = (
                host.store.text.borrow().clone(),
                host.composition_ranges()?,
                host.store.ends.get(),
            );
            if nested {
                assert!(reference.as_ref() == Some(&state));
            } else {
                assert_eq!(state.1.len(), 1);
                reference = Some(state);
            }
            host.tip.shared.client.borrow_mut().take();
            finish(server)
        })?;
    }
    Ok(())
}

#[test]
fn reentry_in_set_text_preserves_keys_and_ack_order() -> Result<()> {
    compare_interrupted_keys(DuringEdit::SetText)
}

#[test]
fn reentry_after_write_lock_preserves_keys_and_ack_order() -> Result<()> {
    compare_interrupted_keys(DuringEdit::AfterWriteLock)
}

#[test]
fn reentry_commands_follow_a_queued_starter_during_commit() -> Result<()> {
    for during_delivery in [false, true] {
        with_host(|host| {
            let server = serve_keys(
                &[
                    Reply::Compose,
                    Reply::Commit,
                    Reply::Compose,
                    Reply::Compose,
                    Reply::Converted,
                    Reply::Compose,
                    Reply::Commit,
                ],
                None,
                &[
                    SpecialKey::None,
                    SpecialKey::Enter,
                    SpecialKey::None,
                    SpecialKey::Backspace,
                    SpecialKey::Space,
                    SpecialKey::Left,
                    SpecialKey::Enter,
                ],
            )?;
            let vk = starter()?;
            press(&host.tip, &host.context, vk)?;
            let keys = host.tip.to_interface();
            let context = host.context.clone();
            let claimed = Rc::new(Cell::new(false));
            let nested = claimed.clone();
            let store = host.store.clone();
            let shared = host.tip.shared.clone();
            host.store.callback.set(Some((
                DuringEdit::AfterWriteLock,
                Box::new(move || {
                    deliver(&keys, &context, vk)?
                        .then_some(())
                        .ok_or_else(failure)?;
                    let commands = move || {
                        let mut all = !during_delivery || shared.pending.borrow().is_empty();
                        for key in [8, 0x20, 0x25, 0x0d] {
                            all &= deliver(&keys, &context, key)?;
                        }
                        nested.set(all);
                        Ok(())
                    };
                    if during_delivery {
                        // The starter has left the queue for its scope check.
                        store
                            .callback
                            .set(Some((DuringEdit::AfterReadLock, Box::new(commands))));
                        Ok(())
                    } else {
                        commands()
                    }
                }),
            )));
            press(&host.tip, &host.context, 0x0d)?;
            assert!(claimed.get(), "a command overtook the queued starter");
            assert!(
                host.store
                    .text
                    .borrow()
                    .iter()
                    .copied()
                    .eq("AB字字".encode_utf16())
            );
            assert!(host.composition_ranges()?.is_empty());
            assert_eq!(HINTS.get(), 0);
            host.tip.shared.client.borrow_mut().take();
            finish(server)
        })?;
    }
    Ok(())
}

#[test]
fn reentry_mode_queue_passes_modifiers_and_untranslated_keys() -> Result<()> {
    with_host(|host| unsafe {
        let server = serve(&[Reply::Commit])?;
        let keys = host.tip.to_interface();
        let context = host.context.clone();
        let passed = Rc::new(Cell::new(false));
        let nested = passed.clone();
        host.store.callback.set(Some((
            DuringEdit::AfterWriteLock,
            Box::new(move || {
                let mut all = deliver(&keys, &context, 0x16)?;
                for vk in [
                    VK_LSHIFT,
                    VK_RSHIFT,
                    VK_LCONTROL,
                    VK_RCONTROL,
                    VK_MENU,
                    VK_LWIN,
                    VK_RWIN,
                    VK_F1,
                ] {
                    let key = KeySource::Wpf.key(usize::from(vk.0), false);
                    let w = WPARAM(key.vk);
                    let l = LPARAM(key.flags as isize);
                    all &= !key.deliver(&keys, &context, false)?;
                    all &= !key.deliver(&keys, &context, true)?;
                    all &= !keys.OnKeyDown(&context, w, l)?.as_bool();
                    all &= !keys.OnKeyDown(&context, w, l)?.as_bool(); // Repeat without Test.
                    set_held(key.vk, false)?;
                    let up = LPARAM((key.flags | 0xc000_0000) as isize);
                    all &= !keys.OnTestKeyUp(&context, w, up)?.as_bool();
                    all &= !keys.OnKeyUp(&context, w, up)?.as_bool();
                }
                nested.set(all);
                Ok(())
            }),
        )));
        press(&host.tip, &host.context, starter()?)?;
        assert!(passed.get(), "a non-input key was claimed after IME ON");
        // Only the mode indicator.
        assert_eq!(HINTS.get(), 1);
        host.tip.shared.client.borrow_mut().take();
        finish(server)
    })
}

#[test]
fn reentry_discard_notice_handles_mouse_input() -> Result<()> {
    use windows::Win32::UI::Controls::*;
    with_host(|_| unsafe {
        let _hint = mode_hint::Hint::for_discard(mode_hint::input_rect())?;
        let window = LAST_HINT.get();
        let mut tool = TTTOOLINFOW {
            cbSize: std::mem::offset_of!(TTTOOLINFOW, lpReserved) as u32,
            ..Default::default()
        };
        assert_ne!(
            SendMessageW(
                window,
                TTM_ENUMTOOLSW,
                Some(WPARAM(0)),
                Some(LPARAM((&raw mut tool) as isize))
            )
            .0,
            0
        );
        assert!(
            tool.hwnd != window || !tool.uFlags.contains(TTF_TRANSPARENT),
            "the notice would forward mouse input to itself"
        );
        SendMessageW(window, WM_MOUSEMOVE, None, None);
        SendMessageW(window, WM_LBUTTONDOWN, Some(WPARAM(1)), None);
        SendMessageW(window, WM_LBUTTONUP, None, None);
        assert!(IsWindow(Some(window)).as_bool());
        Ok(())
    })
}

fn check_discard_position(screen: windows::Win32::Graphics::Gdi::HMONITOR) -> Result<()> {
    use windows::Win32::Graphics::Gdi::*;
    unsafe {
        let mut monitor = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(screen, &mut monitor).ok()?;
        let mut notice = RECT::default();
        GetWindowRect(LAST_HINT.get(), &mut notice)?;
        let gap =
            (windows::Win32::UI::HiDpi::GetDpiForWindow(LAST_HINT.get()).max(96) * 5 / 96) as i32;
        assert_eq!(notice.right + gap, monitor.rcWork.right);
        assert_eq!(notice.bottom + gap, monitor.rcWork.bottom);
    }
    Ok(())
}

#[test]
fn reentry_discard_notice_uses_the_input_monitor_after_focus_loss() -> Result<()> {
    use windows::Win32::Graphics::Gdi::*;
    unsafe extern "system" fn collect(
        screen: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        unsafe { (*(data.0 as *mut Vec<HMONITOR>)).push(screen) };
        true.into()
    }
    with_host(|_| unsafe {
        let field = GetFocus();
        let mut screens: Vec<HMONITOR> = Vec::new();
        EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM((&raw mut screens) as isize),
        )
        .ok()?;
        assert!(!screens.is_empty());
        for screen in screens {
            let mut monitor = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            GetMonitorInfoW(screen, &mut monitor).ok()?;
            SetWindowPos(
                field,
                None,
                monitor.rcWork.left + 16,
                monitor.rcWork.top + 16,
                80,
                40,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )?;
            let _ = SetFocus(Some(field));
            let mut window_rect = RECT::default();
            GetWindowRect(field, &mut window_rect)?;
            assert_eq!(mode_hint::input_rect(), window_rect);

            CreateCaret(field, None, 2, 12)?;
            SetCaretPos(8, 6)?;
            let caret = mode_hint::input_rect();
            assert_eq!(
                (caret.left, caret.top),
                (window_rect.left + 8, window_rect.top + 6)
            );
            DestroyCaret()?;
            let _ = SetFocus(None);
            for rect in [window_rect, caret] {
                let _notice = mode_hint::Hint::for_discard(rect)?;
                check_discard_position(screen)?;
            }
        }
        Ok(())
    })
}

#[test]
fn reentry_rechecks_scope_before_sending_queued_key() -> Result<()> {
    for (private, readonly, native_ui) in [
        (true, false, true),
        (false, false, true),
        (false, true, true),
        (false, false, false),
    ] {
        with_host(|host| {
            host.tip.shared.show_mode_hint.set(native_ui);
            let server = serve(if private {
                &[Reply::Compose, Reply::PrivateCommit]
            } else {
                &[Reply::Converted]
            })?;
            let store = host.store.clone();
            let context = host.context.clone();
            let keys = host.tip.to_interface();
            let taken = Rc::new(Cell::new(false));
            let nested = taken.clone();
            store.refuse_nested_lock.set(true);
            host.store.callback.set(Some((
                DuringEdit::AfterWriteLock,
                Box::new(move || {
                    nested.set(deliver(&keys, &context, 0x0d)?);
                    if readonly {
                        store.readonly.set(true);
                    } else {
                        store.set_scope(if private { IS_PRIVATE } else { IS_PASSWORD });
                    }
                    Ok(())
                }),
            )));
            press(&host.tip, &host.context, starter()?)?;
            assert!(taken.get());
            assert_eq!(HINTS.get(), usize::from(!private && native_ui));
            assert_eq!(HELLOS.get(), 1);
            if !private {
                assert!(host.tip.shared.client.borrow().is_none());
                assert!(
                    host.store
                        .text
                        .borrow()
                        .iter()
                        .copied()
                        .eq("AB漢字".encode_utf16())
                );
                // A password scope still grants the write to end the mark.
                if !readonly {
                    assert_eq!(host.store.compositions.get(), 0);
                    assert_eq!(host.store.ends.get(), 1);
                }
            }
            host.tip.shared.client.borrow_mut().take();
            finish(server)
        })?;
    }
    Ok(())
}

#[test]
fn key_routing_nested_down_passes_a_newly_protected_field() -> Result<()> {
    with_host(|host| unsafe {
        let manager: ITfThreadMgr =
            CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
        let (docs, other) = test_document(&manager, host.owner_id, host.tip.clone())?;
        let server = serve(&[Reply::Compose, Reply::Commit])?;
        let vk = starter()?;
        press(&host.tip, &host.context, vk)?;
        let routes = Rc::new(Cell::new(None));
        let nested = routes.clone();
        host.store.callback.set(Some((
            DuringEdit::AfterWriteLock,
            Box::new(move || {
                let test = other.test(vk)?;
                other.store.set_scope(IS_PASSWORD);
                nested.set(Some((test, other.down(vk)?)));
                Ok(())
            }),
        )));
        press(&host.tip, &host.context, 0x0d)?;
        assert_eq!(routes.get(), Some((true, false)));
        host.check_up(vk, false)?;
        assert_eq!(HINTS.get(), 0);
        host.tip.shared.client.borrow_mut().take();
        finish(server)?;
        docs.Pop(TF_POPF_ALL)
    })
}

#[test]
fn reentry_denied_scope_lock_keeps_the_acknowledged_mark() -> Result<()> {
    with_host(|host| {
        let server = serve_keys(
            &[Reply::Compose, Reply::Commit],
            None,
            &[SpecialKey::None, SpecialKey::Enter],
        )?;
        let context = host.context.clone();
        let keys = host.tip.to_interface();
        let store = host.store.clone();
        let accepted = Rc::new(Cell::new(false));
        let queued = accepted.clone();
        host.store.callback.set(Some((
            DuringEdit::AfterWriteLock,
            Box::new(move || {
                queued.set(deliver(&keys, &context, 8)?);
                store.reject_lock_after.set(Some(0));
                Ok(())
            }),
        )));
        press(&host.tip, &host.context, starter()?)?;
        assert!(accepted.get());
        assert_eq!(host.composition_ranges()?.len(), 1);
        press(&host.tip, &host.context, 0x0d)?;
        assert!(*host.store.text.borrow() == [65, 66, '字' as u16]);
        assert_eq!(HELLOS.get(), 1);
        host.tip.shared.client.borrow_mut().take();
        finish(server)
    })
}

#[test]
fn reentry_declined_enter_and_backspace_report_one_discard() -> Result<()> {
    for (vk, replies, expected, text) in [
        (
            0x0d,
            &[Reply::Compose, Reply::Commit, Reply::Decline][..],
            &[SpecialKey::None, SpecialKey::Enter, SpecialKey::Enter][..],
            "AB字",
        ),
        (
            8,
            &[
                Reply::Compose,
                Reply::Compose,
                Reply::Cancel,
                Reply::Decline,
            ][..],
            &[
                SpecialKey::None,
                SpecialKey::Backspace,
                SpecialKey::Backspace,
                SpecialKey::Backspace,
            ][..],
            "AB",
        ),
    ] {
        with_host(|host| {
            let server = serve_keys(replies, None, expected)?;
            let keys = host.tip.to_interface();
            let context = host.context.clone();
            host.store.callback.set(Some((
                DuringEdit::AfterWriteLock,
                Box::new(move || {
                    // Neither the key nor the discard notice has a focused HWND.
                    let _ = unsafe { SetFocus(None) };
                    for _ in 1..replies.len() {
                        deliver(&keys, &context, vk)?
                            .then_some(())
                            .ok_or_else(failure)?;
                    }
                    Ok(())
                }),
            )));
            press(&host.tip, &host.context, starter()?)?;
            assert!(
                host.store
                    .text
                    .borrow()
                    .iter()
                    .copied()
                    .eq(text.encode_utf16())
            );
            assert_eq!(HINTS.get(), 1);
            unsafe {
                use windows::Win32::Graphics::Gdi::*;
                check_discard_position(MonitorFromPoint(
                    POINT::default(),
                    MONITOR_DEFAULTTOPRIMARY,
                ))?;
            }
            host.tip.shared.client.borrow_mut().take();
            finish(server)
        })?;
    }
    Ok(())
}

#[test]
fn reentry_focus_finishes_the_original_document_once() -> Result<()> {
    for point in [
        DuringEdit::Start,
        DuringEdit::SetText,
        DuringEdit::AfterWriteLock,
    ] {
        with_host(|host| unsafe {
            let manager: ITfThreadMgr =
                CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
            let old = host.context.GetDocumentMgr()?;
            let (new, other) = test_document(&manager, host.owner_id, host.tip.clone())?;
            other.store.text.borrow_mut().clear();
            other.store.selection.set((0, 0));
            manager.SetFocus(&old)?;
            let sink: ITfThreadMgrEventSink = host.tip.to_interface();
            let focused = new.clone();
            let keys = host.tip.to_interface();
            let prior = host.context.clone();
            let shared = host.tip.shared.clone();
            let accepted = Rc::new(Cell::new(false));
            let queued = accepted.clone();
            host.store.callback.set(Some((
                point,
                Box::new(move || {
                    shared.poll_refinement(false);
                    queued.set(deliver(&keys, &prior, 0x0d)?);
                    manager.SetFocus(&focused)?;
                    sink.OnSetFocus(&focused, &old)
                }),
            )));
            let server = serve_keys(
                &[Reply::Converted, Reply::CommitConverted],
                None,
                &[SpecialKey::None, SpecialKey::Enter],
            )?;
            let vk = starter()?;
            assert!(host.test(vk)? && host.down(vk)?);
            assert!(accepted.get());
            assert_eq!(host.store.compositions.get(), 0);
            assert_eq!(host.store.ends.get(), 1);
            assert!(
                host.store
                    .text
                    .borrow()
                    .iter()
                    .copied()
                    .eq("AB漢字".encode_utf16())
            );
            assert!(other.store.text.borrow().is_empty());
            finish(server)?;
            check_connected_commit(&other.tip, &other.context)?;
            assert!(*other.store.text.borrow() == ['字' as u16]);
            new.Pop(TF_POPF_ALL)
        })?;
    }
    Ok(())
}

#[test]
fn reentry_starters_before_and_after_termination_use_a_fresh_session() -> Result<()> {
    with_host(|host| unsafe {
        let original = serve(&[Reply::Compose])?;
        let vk = starter()?;
        assert!(host.test(vk)?);
        let fresh = serve_keys(
            &[Reply::Compose, Reply::Commit],
            None,
            &[SpecialKey::None; 2],
        )?;
        let owner: ITfContextOwnerCompositionServices = host.context.cast()?;
        let keys = host.tip.to_interface();
        let context = host.context.clone();
        let taken = Rc::new(Cell::new((false, false)));
        let nested = taken.clone();
        host.store.callback.set(Some((
            DuringEdit::AfterWriteLock,
            Box::new(move || {
                let first = deliver(&keys, &context, vk)?;
                owner.TerminateComposition(None)?;
                nested.set((first, deliver(&keys, &context, vk)?));
                Ok(())
            }),
        )));
        assert!(host.down(vk)?);
        assert_eq!(taken.get(), (true, true));
        assert_eq!(HELLOS.get(), 2);
        assert!(
            host.store
                .text
                .borrow()
                .iter()
                .copied()
                .eq("ABじ字".encode_utf16())
        );
        host.tip.shared.client.borrow_mut().take();
        finish(original)?;
        finish(fresh)
    })
}

#[test]
fn reentry_rejected_edit_processes_unsubmitted_key() -> Result<()> {
    with_host(|host| {
        let original = serve_edits(&[Reply::Compose], Some(1))?;
        let vk = starter()?;
        assert!(host.test(vk)?);
        let fresh = serve(&[Reply::Commit])?;
        let keys = host.tip.to_interface();
        let context = host.context.clone();
        host.store.refusal.set(Some(Refusal::SetText));
        host.store.callback.set(Some((
            DuringEdit::Start,
            Box::new(move || {
                deliver(&keys, &context, vk)?
                    .then_some(())
                    .ok_or_else(failure)
            }),
        )));
        assert!(host.down(vk)?);
        assert_eq!(HELLOS.get(), 2);
        assert!(
            host.store
                .text
                .borrow()
                .iter()
                .copied()
                .eq("AB字".encode_utf16())
        );
        host.tip.shared.client.borrow_mut().take();
        finish(original)?;
        finish(fresh)
    })
}

#[test]
fn reentry_foreground_loss_drains_the_attached_store() -> Result<()> {
    for source in [KeySource::Wpf, KeySource::Hook] {
        with_host(|host| unsafe {
            let original = serve_keys(
                &[Reply::Compose, Reply::Converted, Reply::Commit],
                None,
                &[SpecialKey::None, SpecialKey::None, SpecialKey::Enter],
            )?;
            let vk = starter()?;
            let keys: ITfKeyEventSink = host.tip.to_interface();
            let context = host.context.clone();
            let enter = source.key(0x0d, false);
            host.store.callback.set(Some((
                DuringEdit::AfterWriteLock,
                Box::new(move || {
                    deliver(&keys, &context, vk)?
                        .then_some(())
                        .ok_or_else(failure)?;
                    enter
                        .deliver(&keys, &context, false)?
                        .then_some(())
                        .ok_or_else(failure)?;
                    keys.OnSetFocus(false)
                }),
            )));
            assert!(host.test(vk)? && host.down(vk)?);
            enter.check_up(host, true)?;
            host.check_up(vk, true)?;
            assert_eq!(HINTS.get(), 0);
            assert_eq!(HELLOS.get(), 1);
            assert!(
                host.store
                    .text
                    .borrow()
                    .iter()
                    .copied()
                    .eq("AB字".encode_utf16())
            );
            finish(original)
        })?;
    }
    Ok(())
}

#[test]
fn reentry_failed_ack_restores_before_the_unsubmitted_key() -> Result<()> {
    with_host(|host| {
        let original = serve(&[Reply::UnackedCompose])?;
        let vk = starter()?;
        assert!(host.test(vk)?);
        let fresh = serve(&[Reply::Restore, Reply::Commit])?;
        let keys = host.tip.to_interface();
        let context = host.context.clone();
        host.store.callback.set(Some((
            DuringEdit::SetText,
            Box::new(move || {
                FAIL_ACK.set(true);
                deliver(&keys, &context, vk)?
                    .then_some(())
                    .ok_or_else(failure)
            }),
        )));
        assert!(host.down(vk)?);
        assert_eq!(HELLOS.get(), 2);
        assert!(
            host.store
                .text
                .borrow()
                .iter()
                .copied()
                .eq("AB字".encode_utf16())
        );
        host.tip.shared.client.borrow_mut().take();
        finish(original)?;
        finish(fresh)
    })
}

#[test]
fn reentry_enter_keeps_the_conversion_after_a_lost_ack() -> Result<()> {
    with_host(|host| {
        let original = serve(&[Reply::ConvertedWithoutAck])?;
        let vk = starter()?;
        assert!(host.test(vk)?);
        let fresh = serve(&[])?;
        let keys = host.tip.to_interface();
        let context = host.context.clone();
        host.store.callback.set(Some((
            DuringEdit::SetText,
            Box::new(move || {
                FAIL_ACK.set(true);
                deliver(&keys, &context, 0x0d)?
                    .then_some(())
                    .ok_or_else(failure)
            }),
        )));
        assert!(host.down(vk)?);
        assert!(
            host.store
                .text
                .borrow()
                .iter()
                .copied()
                .eq("AB漢字".encode_utf16())
        );
        assert!(host.composition_ranges()?.is_empty());
        host.tip.shared.client.borrow_mut().take();
        finish(original)?;
        finish(fresh)
    })
}

#[test]
fn reentry_lost_reply_shows_hint_and_the_next_success_clears_it() -> Result<()> {
    with_host(|host| {
        let original = serve(&[Reply::Commit, Reply::Lose])?;
        let vk = starter()?;
        assert!(host.test(vk)?);
        let keys = host.tip.to_interface();
        let context = host.context.clone();
        host.store.callback.set(Some((
            DuringEdit::SetText,
            Box::new(move || {
                deliver(&keys, &context, vk)?
                    .then_some(())
                    .ok_or_else(failure)
            }),
        )));
        assert!(host.down(vk)?);
        assert_eq!(HINTS.get(), 1);
        assert_eq!(START_PROBES.get(), 1);
        finish(original)?;
        let fresh = serve(&[Reply::Commit])?;
        assert!(host.tip.shared.connect(false) == Connection::Attached);
        let keys = host.tip.to_interface();
        host.tip
            .shared
            .with_client(&host.context, |_, _| deliver(&keys, &host.context, vk))
            .ok_or_else(failure)??
            .then_some(())
            .ok_or_else(failure)?;
        assert!(!unsafe { IsWindowVisible(LAST_HINT.get()) }.as_bool());
        host.tip.shared.client.borrow_mut().take();
        finish(fresh)
    })
}

#[test]
fn reentry_mode_switch_follows_queued_text() -> Result<()> {
    for (mode_key, turn_on_again) in [
        (0xf3, false),
        (0xf4, false),
        (0x19, false),
        (0x1a, false),
        (0xc0, false),
        (0x1a, true),
    ] {
        with_host(|host| unsafe {
            let manager: ITfThreadMgr =
                CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
            let mode = input_mode::Mode::attach(
                &manager,
                host.tip.shared.tid.get(),
                &host.tip.to_interface(),
            )?;
            let switch = mode.switch();
            let original = switch.japanese();
            host.tip.mode.replace(Some(mode));
            switch.apply(host.tip.shared.tid.get(), input_mode::Command::On)?;
            let keys: ITfKeyEventSink = host.tip.to_interface();
            let context = host.context.clone();
            let vk = starter()?;
            let claimed = Rc::new(Cell::new(false));
            let after_mode = claimed.clone();
            host.store.callback.set(Some((
                DuringEdit::SetText,
                Box::new(move || {
                    deliver(&keys, &context, vk)?
                        .then_some(())
                        .ok_or_else(failure)?;
                    if mode_key == 0xc0 {
                        let toggle = GUID::from_u128(0x8a1c7f52_9027_4b3d_833d_c2bbf31000c0);
                        keys.OnPreservedKey(&context, &toggle)?.ok()
                    } else {
                        deliver(&keys, &context, mode_key)?
                            .then_some(())
                            .ok_or_else(failure)
                    }?;
                    after_mode
                        .set(deliver(&keys, &context, vk)? && deliver(&keys, &context, 0x0d)?);
                    if turn_on_again {
                        for key in [0x16, vk] {
                            deliver(&keys, &context, key)?
                                .then_some(())
                                .ok_or_else(failure)?;
                        }
                    }
                    Ok(())
                }),
            )));
            let server = serve(&[Reply::Compose, Reply::Commit])?;
            assert!(host.tip.shared.connect(false) == Connection::Attached);
            let second = turn_on_again
                .then(|| serve(&[Reply::CommitConverted]))
                .transpose()?;
            press(&host.tip, &host.context, vk)?;
            let off = !switch.japanese();
            let expected = if turn_on_again {
                "AB字漢字"
            } else {
                "AB字"
            };
            let correct = host
                .store
                .text
                .borrow()
                .iter()
                .copied()
                .eq(expected.encode_utf16());
            let post_queue_claim = host.test(vk)?;
            host.check_up(0x0d, true)?;
            switch.apply(
                host.tip.shared.tid.get(),
                if original {
                    input_mode::Command::On
                } else {
                    input_mode::Command::Off
                },
            )?;
            assert_eq!(off, !turn_on_again);
            assert!(correct);
            assert!(
                claimed.get(),
                "keys following the mode switch overtook the queue"
            );
            assert_eq!(HINTS.get(), 2 + usize::from(turn_on_again));
            assert_eq!(post_queue_claim, turn_on_again);
            if let Some(second) = second {
                host.tip.shared.client.borrow_mut().take();
                finish(second)?;
            }
            finish(server)
        })?;
    }
    Ok(())
}

#[test]
fn reentry_focus_drains_each_fields_starter_in_its_session() -> Result<()> {
    for terminate_on_move in [false, true] {
        with_host(|host| unsafe {
            let manager: ITfThreadMgr =
                CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
            let previous = host.context.GetDocumentMgr()?;
            let (next, other) = test_document(&manager, host.owner_id, host.tip.clone())?;
            manager.SetFocus(&previous)?;
            host.tip.shared.manager.replace(Some(manager.clone()));
            let mut endpoint = listener()?;
            let (focus_result, moved) = std::sync::mpsc::channel::<bool>();
            let server = std::thread::spawn(move || {
                // SetFocus can end the composition itself. Only that case needs a
                // new connection for the unsubmitted key in the original field.
                let mut owner_ended = false;
                let next = moved.into_iter().take(1).filter_map(|ended| {
                    owner_ended = ended;
                    (!ended).then_some(Reply::NormalCommit)
                });
                reply_to_session(
                    accept_session(&mut endpoint)?,
                    std::iter::once(Reply::Converted).chain(next),
                    None,
                    &[],
                )?;
                if owner_ended {
                    reply_to_session(
                        accept_session(&mut endpoint)?,
                        [Reply::NormalCommit],
                        None,
                        &[],
                    )?;
                }
                reply_to_session(
                    accept_session(&mut endpoint)?,
                    [Reply::PrivateCompose, Reply::Commit],
                    None,
                    &[SpecialKey::None, SpecialKey::Enter],
                )
            });
            let vk = starter()?;
            assert!(host.test(vk)?);
            let keys = host.tip.to_interface();
            let events: ITfThreadMgrEventSink = host.tip.to_interface();
            let old = host.context.clone();
            let new = other.context.clone();
            let store = other.store.clone();
            let old_store = host.store.clone();
            let owner_ended = Rc::new(Cell::new(false));
            let ended = owner_ended.clone();
            let ownership = Rc::new(Cell::new((true, false)));
            let new_field = ownership.clone();
            other.store.set_scope(IS_PRIVATE);
            host.store.callback.set(Some((
                DuringEdit::AfterWriteLock,
                Box::new(move || {
                    deliver(&keys, &old, vk)?
                        .then_some(())
                        .ok_or_else(failure)?;
                    let focused = new.GetDocumentMgr()?;
                    manager.SetFocus(&focused)?;
                    if terminate_on_move && old_store.ends.get() == 0 {
                        old.cast::<ITfContextOwnerCompositionServices>()?
                            .TerminateComposition(None)?;
                    }
                    events.OnSetFocus(&focused, &previous)?;
                    ended.set(old_store.ends.get() != 0);
                    focus_result.send(ended.get()).map_err(|_| failure())?;
                    let before = deliver(&keys, &new, 0x0d)?;
                    deliver(&keys, &new, vk)?
                        .then_some(())
                        .ok_or_else(failure)?;
                    new_field.set((before, deliver(&keys, &new, 0x0d)?));
                    store.set_scope(IS_DEFAULT);
                    // Cleanup of the previous document can follow the new key test.
                    events.OnUninitDocumentMgr(&previous)
                }),
            )));
            assert!(host.down(vk)?);
            assert_eq!(ownership.get(), (false, true));
            assert_eq!(host.store.ends.get(), 1 + u32::from(owner_ended.get()));
            let expected = if owner_ended.get() {
                "AB漢字字"
            } else {
                "AB字"
            };
            assert!(
                host.store
                    .text
                    .borrow()
                    .iter()
                    .copied()
                    .eq(expected.encode_utf16())
            );
            assert!(
                other
                    .store
                    .text
                    .borrow()
                    .iter()
                    .copied()
                    .eq("AB字".encode_utf16())
            );
            assert_eq!(HELLOS.get(), 2 + usize::from(owner_ended.get()));
            assert_eq!(HINTS.get(), 0);
            host.tip.shared.client.borrow_mut().take();
            finish(server)?;
            next.Pop(TF_POPF_ALL)
        })?;
    }
    Ok(())
}

#[test]
fn reentry_departing_edit_preserves_the_hosts_replacement() -> Result<()> {
    with_host(|host| unsafe {
        let server = serve_edits(&[Reply::Converted, Reply::Commit], Some(2))?;
        let vk = starter()?;
        let keys: ITfKeyEventSink = host.tip.to_interface();
        let context = host.context.clone();
        let store = host.store.clone();
        host.store.callback.set(Some((
            DuringEdit::AfterWriteLock,
            Box::new(move || {
                deliver(&keys, &context, vk)?
                    .then_some(())
                    .ok_or_else(failure)?;
                keys.OnSetFocus(false)?;
                // The owner edits the marked range without ending the composition.
                *store.text.borrow_mut().last_mut().ok_or_else(failure)? = 67;
                let sink = store.sink.borrow().clone().ok_or_else(failure)?;
                sink.OnTextChange(
                    TEXT_STORE_TEXT_CHANGE_FLAGS(0),
                    &TS_TEXTCHANGE {
                        acpStart: 3,
                        acpOldEnd: 4,
                        acpNewEnd: 4,
                    },
                )
            }),
        )));
        assert!(host.test(vk)? && host.down(vk)?);
        assert!(
            host.store
                .text
                .borrow()
                .iter()
                .copied()
                .eq("AB漢C".encode_utf16())
        );
        assert_eq!(host.store.compositions.get(), 0);
        assert_eq!(HINTS.get(), 1);
        finish(server)
    })
}

fn test_candidate() -> ipc::CandidateHandle {
    ipc::CandidateHandle {
        session_epoch: 1,
        candidate_revision: 1,
        slot: 0,
    }
}

fn check_refused_candidate_read(action: fn(&Rc<Shared>), expected_discards: usize) -> Result<()> {
    for queued in [true, false] {
        with_host(|host| unsafe {
            let manager: ITfThreadMgr =
                CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
            manager.SetFocus(&host.context.GetDocumentMgr()?)?;
            let shared = &host.tip.shared;
            shared.manager.replace(Some(manager));
            let server = serve(if queued {
                &[Reply::Candidates, Reply::Candidates, Reply::Select]
            } else {
                &[Reply::Candidates, Reply::Select]
            })?;
            let vk = starter()?;
            press(&host.tip, &host.context, vk)?;
            let mark = shared.composition.borrow().clone().ok_or_else(failure)?;
            let displayed = host.store.text.borrow().clone();
            let field = shared.field.get();
            let candidate_owner = shared.clone();
            let store = host.store.clone();
            let refuse_read = move || {
                store.reject_lock_after.set(Some(0));
                action(&candidate_owner);
                Ok(())
            };
            if queued {
                host.store
                    .callback
                    .set(Some((DuringEdit::AfterWriteLock, Box::new(refuse_read))));
                press(&host.tip, &host.context, vk)?;
            } else {
                refuse_read()?;
            }

            assert_eq!(host.store.reject_lock_after.get(), None);
            assert_eq!(host.store.compositions.get(), 1);
            assert_eq!(host.store.ends.get(), 0);
            assert!(shared.composition.borrow().as_ref() == Some(&mark));
            assert!(host.store.text.borrow().as_slice() == displayed);
            assert!(shared.field.get() == field);
            assert!(shared.page.borrow().refinement_pending);
            assert_eq!(HINTS.get(), expected_discards);
            // The same candidate can still be selected on the original session.
            shared.choose(test_candidate());
            assert_eq!(host.store.ends.get(), 1);
            assert_eq!(HELLOS.get(), 1);
            shared.client.borrow_mut().take();
            finish(server)
        })?;
    }
    Ok(())
}

#[test]
fn candidate_click_keeps_the_mark_after_a_refused_read() -> Result<()> {
    check_refused_candidate_read(|shared| shared.choose(test_candidate()), 1)
}

#[test]
fn candidate_poll_keeps_the_mark_after_a_refused_read() -> Result<()> {
    check_refused_candidate_read(|shared| shared.poll_refinement(true), 0)
}

#[test]
fn reentry_stale_candidate_click_reports_discard() -> Result<()> {
    with_host(|host| {
        let server = serve(&[Reply::Candidates, Reply::Commit])?;
        let vk = starter()?;
        press(&host.tip, &host.context, vk)?;
        let shared = host.tip.shared.clone();
        host.store.callback.set(Some((
            DuringEdit::SetText,
            Box::new(move || {
                shared.choose(test_candidate());
                Ok(())
            }),
        )));
        press(&host.tip, &host.context, vk)?;
        assert_eq!(HINTS.get(), 1);
        host.tip.shared.client.borrow_mut().take();
        finish(server)
    })
}

#[test]
fn reentry_departing_candidate_click_precedes_enter() -> Result<()> {
    with_host(|host| {
        let server = serve(&[
            Reply::Candidates,
            Reply::Candidates,
            Reply::Select,
            Reply::Decline,
        ])?;
        let vk = starter()?;
        press(&host.tip, &host.context, vk)?;
        let keys: ITfKeyEventSink = host.tip.to_interface();
        let context = host.context.clone();
        let shared = host.tip.shared.clone();
        host.store.callback.set(Some((
            DuringEdit::SetText,
            Box::new(move || {
                shared.choose(test_candidate());
                deliver(&keys, &context, 0x0d)?
                    .then_some(())
                    .ok_or_else(failure)?;
                unsafe { keys.OnSetFocus(false) }
            }),
        )));
        assert!(host.test(vk)? && host.down(vk)?);
        assert!(
            host.store
                .text
                .borrow()
                .iter()
                .copied()
                .eq("AB字".encode_utf16())
        );
        assert_eq!(HINTS.get(), 1); // Enter was declined after the click committed.
        assert_eq!(HELLOS.get(), 1);
        finish(server)
    })
}

#[test]
fn reentry_candidate_click_finalize_and_expired_poll_follow_the_ack() -> Result<()> {
    #[derive(Clone, Copy)]
    enum Callback {
        Click,
        Finalize,
        Timeout,
    }
    for action in [Callback::Click, Callback::Finalize, Callback::Timeout] {
        with_host(|host| unsafe {
            let manager: ITfThreadMgr =
                CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
            manager.SetFocus(&host.context.GetDocumentMgr()?)?;
            host.tip.shared.manager.replace(Some(manager));
            let server = serve(if matches!(action, Callback::Timeout) {
                &[Reply::Candidates, Reply::Candidates, Reply::Poll]
            } else {
                &[Reply::Candidates, Reply::Candidates, Reply::Select]
            })?;
            let vk = starter()?;
            press(&host.tip, &host.context, vk)?;
            let shared = host.tip.shared.clone();
            host.store.callback.set(Some((
                DuringEdit::SetText,
                Box::new(move || {
                    match action {
                        Callback::Click => shared.choose(test_candidate()),
                        Callback::Finalize => {
                            let behavior: ITfCandidateListUIElementBehavior =
                                uiless::Element::new(&shared).into();
                            behavior.Finalize()?;
                        }
                        Callback::Timeout => shared.poll_refinement(true),
                    }
                    Ok(())
                }),
            )));
            press(&host.tip, &host.context, vk)?;
            if matches!(action, Callback::Timeout) {
                let page = host.tip.shared.page.borrow();
                assert!(!page.refinement_pending);
                assert_eq!(
                    page.refinement_outcome,
                    ipc::RefinementOutcome::TimedOut as i32
                );
            } else {
                assert!(
                    host.store
                        .text
                        .borrow()
                        .iter()
                        .copied()
                        .eq("AB字".encode_utf16())
                );
            }
            host.tip.shared.client.borrow_mut().take();
            finish(server)
        })?;
    }
    Ok(())
}

#[test]
fn reentry_owner_termination_discards_the_checked_out_client() -> Result<()> {
    for point in [DuringEdit::SetText, DuringEdit::AfterWriteLock] {
        with_host(|host| unsafe {
            let owner: ITfContextOwnerCompositionServices = host.context.cast()?;
            let store = host.store.clone();
            host.store.callback.set(Some((
                point,
                Box::new(move || {
                    match owner.TerminateComposition(None) {
                        Err(e) if point == DuringEdit::SetText && e.code() == TF_E_NOLOCK => {
                            // The owner can regain its lock before the TIP's
                            // RequestEditSession returns.
                            store.callback.set(Some((
                                DuringEdit::AfterWriteLock,
                                Box::new(move || owner.TerminateComposition(None)),
                            )));
                            Ok(())
                        }
                        result => result,
                    }
                }),
            )));
            let server = serve(&[Reply::Converted])?;
            press(&host.tip, &host.context, starter()?)?;
            assert_eq!(host.store.compositions.get(), 0);
            assert!(host.tip.shared.client.borrow().is_none());
            assert!(host.tip.shared.preedit.borrow().segments.is_empty());
            finish(server)?;
            check_connected_commit(&host.tip, &host.context)?;
            assert!(
                host.store
                    .text
                    .borrow()
                    .iter()
                    .copied()
                    .eq("AB漢字字".encode_utf16())
            );
            Ok(())
        })?;
    }
    Ok(())
}

#[test]
fn reentry_pop_discards_the_detached_document() -> Result<()> {
    with_host(|host| unsafe {
        let manager: ITfThreadMgr =
            CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
        let (docs, old) = test_document(&manager, host.owner_id, host.tip.clone())?;
        manager.SetFocus(&docs)?;
        host.tip.shared.manager.replace(Some(manager.clone()));
        let sink: ITfThreadMgrEventSink = host.tip.to_interface();
        let source: ITfSource = manager.cast()?;
        let cookie = source.AdviseSink(&ITfThreadMgrEventSink::IID, &sink)?;
        let keys = old.tip.to_interface();
        let context = old.context.clone();
        let vk = starter()?;
        let popping = docs.clone();
        let pop_succeeded = Rc::new(Cell::new(false));
        let popped = pop_succeeded.clone();
        old.store.callback.set(Some((
            DuringEdit::AfterWriteLock,
            Box::new(move || {
                deliver(&keys, &context, vk)?
                    .then_some(())
                    .ok_or_else(failure)?;
                popping.Pop(TF_POPF_ALL)?;
                popped.set(true);
                Ok(())
            }),
        )));
        let server = serve(&[Reply::Converted])?;
        assert!(old.test(vk)? && old.down(vk)?);
        assert!(pop_succeeded.get(), "the host did not pop the context");
        let contexts = docs.EnumContexts()?;
        let mut entry = [None];
        let mut count = 0;
        loop {
            contexts.Next(&mut entry, &mut count)?;
            if count == 0 {
                break;
            }
            assert!(entry[0].take().as_ref() != Some(&old.context));
        }
        assert!(
            old.store
                .text
                .borrow()
                .iter()
                .copied()
                .eq("AB漢字".encode_utf16())
        );
        source.UnadviseSink(cookie)?;
        assert_eq!(HINTS.get(), 1);
        assert!(host.tip.shared.client.borrow().is_none());
        assert!(host.tip.shared.composition.borrow().is_none());
        finish(server)?;
        manager.SetFocus(&host.context.GetDocumentMgr()?)?;
        check_connected_commit(&host.tip, &host.context)?;
        assert!(*host.store.text.borrow() == [65, 66, '字' as u16]);
        Ok(())
    })
}

#[test]
fn reentry_ignores_notifications_for_the_previous_document_and_mark() -> Result<()> {
    with_host(|host| unsafe {
        let manager: ITfThreadMgr =
            CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
        let (docs, old) = test_document(&manager, host.owner_id, host.tip.clone())?;
        let server = serve(&[Reply::Compose])?;
        let vk = starter()?;
        press(&old.tip, &old.context, vk)?;
        let previous = old
            .tip
            .shared
            .composition
            .borrow()
            .clone()
            .ok_or_else(failure)?;
        let events: ITfThreadMgrEventSink = host.tip.to_interface();
        let current = host.context.GetDocumentMgr()?;
        events.OnSetFocus(&current, &docs)?;
        finish(server)?;
        let ended: ITfCompositionSink = host.tip.to_interface();
        let source = old.context.clone();
        let previous_manager = docs.clone();
        host.store.callback.set(Some((
            DuringEdit::SetText,
            Box::new(move || {
                ended.OnCompositionTerminated(0, &previous)?;
                events.OnPopContext(&source)?;
                events.OnUninitDocumentMgr(&previous_manager)?;
                events.OnSetFocus(&current, &previous_manager)
            }),
        )));
        check_connected_commit(&host.tip, &host.context)?;
        assert!(host.composition_ranges()?.is_empty());
        assert!(*host.store.text.borrow() == [65, 66, '字' as u16]);
        docs.Pop(TF_POPF_ALL)
    })
}

fn check_rejected_edit(refusal: Refusal, steps: &'static [Reply]) -> Result<()> {
    with_host(|host| {
        let server = serve_edits(steps, Some(steps.len() as u64))?;
        let vk = starter()?;
        let composing = steps.len() == 2;
        if composing {
            press(&host.tip, &host.context, vk)?;
            assert_eq!(host.store.compositions.get(), 1);
        }
        host.store.refusal.set(Some(refusal));
        let rejected_key = match steps.last() {
            Some(Reply::Cancel) => 0x1b,
            Some(Reply::Commit) => 0x0d,
            _ => vk,
        };
        press(&host.tip, &host.context, rejected_key)?;
        assert!(host.store.refusal.get().is_none());
        assert_eq!(host.store.compositions.get(), 0);
        assert_eq!(host.store.end(), if composing { 3 } else { 2 });
        // The negative ACK ends the failed session. The next key opens its own.
        finish(server)?;
        let fresh = serve(&[Reply::Commit])?;
        press(&host.tip, &host.context, vk)?;
        assert_eq!(HELLOS.get(), 2);
        assert!(host.store.text.borrow().ends_with(&['字' as u16]));
        assert_eq!(host.store.compositions.get(), 0);
        host.tip.shared.client.borrow_mut().take();
        finish(fresh)
    })
}

#[test]
fn edit_recovery_after_write_lock_refusal() -> Result<()> {
    check_rejected_edit(Refusal::WriteLock, &[Reply::Compose])?;
    check_rejected_edit(Refusal::WriteLock, &[Reply::Compose, Reply::Commit])
}

#[test]
fn edit_recovery_after_start_refusal() -> Result<()> {
    check_rejected_edit(Refusal::Start, &[Reply::Compose])
}

#[test]
fn edit_recovery_after_text_refusal() -> Result<()> {
    check_rejected_edit(Refusal::SetText, &[Reply::Compose])
}

#[test]
fn edit_recovery_after_cancel_refusal() -> Result<()> {
    check_rejected_edit(Refusal::SetText, &[Reply::Compose, Reply::Cancel])
}

#[test]
fn edit_recovery_acknowledges_text_despite_caret_failure() -> Result<()> {
    with_host(|host| {
        let server = serve(&[Reply::Compose, Reply::Commit])?;
        for (vk, composing) in [(starter()?, true), (0x0d, false)] {
            host.store.refusal.set(Some(Refusal::SetSelection));
            press(&host.tip, &host.context, vk)?;
            assert!(host.store.refusal.get().is_none());
            assert_eq!(host.store.compositions.get(), u32::from(composing));
        }
        assert!(*host.store.text.borrow() == [65, 66, '字' as u16]);
        host.tip.shared.client.borrow_mut().take();
        finish(server)
    })
}

#[test]
fn edit_recovery_after_owner_termination() -> Result<()> {
    for focus_loss in [false, true] {
        with_host(|host| unsafe {
            let server = serve(&[Reply::Compose])?;
            let vk = starter()?;
            press(&host.tip, &host.context, vk)?;
            host.context
                .cast::<ITfContextOwnerCompositionServices>()?
                .TerminateComposition(None)?;
            assert_eq!(host.store.compositions.get(), 0);
            if focus_loss {
                let keys: ITfKeyEventSink = host.tip.to_interface();
                keys.OnSetFocus(false)?;
                keys.OnSetFocus(true)?;
            }
            assert!(*host.store.text.borrow() == [65, 66, 'じ' as u16]);
            let fresh = serve(&[Reply::Commit])?;
            press(&host.tip, &host.context, vk)?;
            assert!(*host.store.text.borrow() == [65, 66, 'じ' as u16, '字' as u16]);
            host.tip.shared.client.borrow_mut().take();
            finish(server)?;
            finish(fresh)
        })?;
    }
    Ok(())
}

#[test]
fn edit_recovery_preserves_owner_changes_on_focus_loss() -> Result<()> {
    for changed in [false, true] {
        with_host(|host| {
            let server = serve(&[Reply::Converted])?;
            press(&host.tip, &host.context, starter()?)?;
            assert!(*host.store.text.borrow() == [65, 66, '漢' as u16, '字' as u16]);
            if changed {
                *host
                    .store
                    .text
                    .borrow_mut()
                    .last_mut()
                    .ok_or_else(failure)? = 67;
                let sink = host.store.sink.borrow().clone().ok_or_else(failure)?;
                unsafe {
                    sink.OnTextChange(
                        TEXT_STORE_TEXT_CHANGE_FLAGS(0),
                        &TS_TEXTCHANGE {
                            acpStart: 3,
                            acpOldEnd: 4,
                            acpNewEnd: 4,
                        },
                    )?;
                }
                assert_eq!(host.store.compositions.get(), 1);
            }
            let keys: ITfKeyEventSink = host.tip.to_interface();
            unsafe { keys.OnSetFocus(false)? };
            let expected = if changed { "AB漢C" } else { "ABかんじ" };
            assert!(
                host.store
                    .text
                    .borrow()
                    .iter()
                    .copied()
                    .eq(expected.encode_utf16())
            );
            assert_eq!(host.store.compositions.get(), 0);
            finish(server)
        })?;
    }
    Ok(())
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

fn with_cuas_edit(check: fn(HWND, &ComObject<Tip>, &ITfContext) -> Result<()>) -> Result<()> {
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
            check(edit, &test_tip(&manager)?, &context)
        })();
        DestroyWindow(edit)?;
        result
    }
}

// CUAS posts messages to update the window after the TSF edit session.
#[track_caller]
fn wait_for_text(window: HWND, expected: &str) {
    let expected: Vec<_> = expected.encode_utf16().collect();
    let start = std::time::Instant::now();
    // Room for text the control should not hold, so a failure shows it.
    let mut text = vec![0; expected.len() + 32];
    loop {
        dispatch_pending();
        let count = unsafe { GetWindowTextW(window, &mut text) } as usize;
        if text.get(..count) == Some(expected.as_slice()) {
            return;
        }
        // The control holds fixed test strings only.
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "unexpected control text: {:?}, expected {:?}",
            String::from_utf16_lossy(text.get(..count).unwrap_or_default()),
            String::from_utf16_lossy(&expected)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Whether the application's end of the composition reached the text service
/// within a short wait.
fn ended_by_application(tip: &ComObject<Tip>) -> bool {
    let start = std::time::Instant::now();
    loop {
        dispatch_pending();
        if tip.shared.composition.borrow().is_none() {
            return true;
        }
        if start.elapsed() >= Duration::from_millis(500) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn ordinary_win32_edit_converts_through_cuas_on_a_hidden_desktop() -> Result<()> {
    use std::io::Write;
    use windows::Win32::UI::Input::Ime::{CPS_CANCEL, CPS_COMPLETE};
    if std::env::var_os("KANADE_TEST_SKIP_CUAS_EDIT").as_deref() == Some("1".as_ref()) {
        writeln!(
            std::io::stderr().lock(),
            "Skipping Win32 EDIT conversion: KANADE_TEST_SKIP_CUAS_EDIT=1"
        )?;
        return Ok(());
    }
    // CUAS keeps process-wide desktop resources. Exercise the same EDIT for
    // all three scenarios, clearing the committed text between them.
    hidden_desktop(|| {
        with_cuas_edit(|edit, tip, context| {
            check_connected_commit(tip, context)?;
            wait_for_text(edit, "字");
            for action in [CPS_COMPLETE, CPS_CANCEL] {
                unsafe { SetWindowTextW(edit, windows::core::w!(""))? };
                dispatch_pending();
                check_imm_termination(edit, tip, context, action)?;
            }
            Ok(())
        })
    })
}

fn check_imm_termination(
    edit: HWND,
    tip: &ComObject<Tip>,
    context: &ITfContext,
    action: windows::Win32::UI::Input::Ime::NOTIFY_IME_INDEX,
) -> Result<()> {
    use windows::Win32::UI::Input::Ime::*;
    let server = serve(&[Reply::Compose])?;
    press(tip, context, starter()?)?;
    dispatch_pending();
    unsafe {
        let imc = ImmGetContext(edit);
        assert!(!imc.is_invalid());
        let notified = ImmNotifyIME(imc, NI_COMPOSITIONSTR, action, 0);
        ImmReleaseContext(edit, imc).ok()?;
        notified.ok()?;
    }
    let complete = action == CPS_COMPLETE;
    wait_for_text(edit, if complete { "じ" } else { "" });
    // Windows 11 tells the text service that the application ended the
    // composition; Windows Server 2022 does not. What the user sees is the
    // text, so the checks below look at the control and not at the service.
    if !ended_by_application(tip) {
        let mut err = std::io::stderr().lock();
        let _ = std::io::Write::write_all(
            &mut err,
            b"The IMM completion did not reach the text service as an ended composition\n",
        );
    }
    // Also exercise a focus notification arriving after the app cancelled.
    if !complete {
        let keys: ITfKeyEventSink = tip.to_interface();
        unsafe {
            keys.OnSetFocus(false)?;
            keys.OnSetFocus(true)?;
        }
    }
    let fresh = serve(&[Reply::Commit])?;
    press(tip, context, starter()?)?;
    wait_for_text(edit, if complete { "じ字" } else { "字" });
    tip.shared.client.borrow_mut().take();
    finish(server)?;
    finish(fresh)
}

fn hidden_desktop(check: impl FnOnce() -> Result<()> + Send + 'static) -> Result<()> {
    use windows::Win32::System::StationsAndDesktops::*;
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let name = windows::core::HSTRING::from(format!(
        "KanadeTipTest-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
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
        let result = check();
        CoUninitialize();
        result
    })
    .join();
    let closed = unsafe { CloseDesktop(desktop) }
        .map_err(|e| Error::new(e.code(), "could not close the hidden desktop"));
    result.unwrap_or_else(|panic| std::panic::resume_unwind(panic))?;
    closed
}

fn check_richedit_modules() -> Result<()> {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    if unsafe { GetModuleHandleW(windows::core::w!("kanade_tip.dll")) }.is_ok() {
        Err(Error::new(
            E_FAIL,
            "RichEdit loaded the installed Kanade TIP",
        ))
    } else {
        Ok(())
    }
}

fn rich_edit_control(parent: HWND, text: PCWSTR) -> Result<HWND> {
    use windows::Win32::UI::Controls::RichEdit::*;
    unsafe {
        let edit = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            MSFTEDIT_CLASS,
            text,
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            0,
            0,
            160,
            40,
            Some(parent),
            None,
            None,
            None,
        )?;
        let style = SendMessageW(
            edit,
            EM_SETEDITSTYLE,
            Some(WPARAM(SES_USECTF as usize)),
            Some(LPARAM(SES_USECTF as isize)),
        );
        assert_ne!(style.0 & SES_USECTF as isize, 0);
        check_richedit_modules()?;
        Ok(edit)
    }
}

fn with_richedit(
    check: impl FnOnce(HWND, HWND, ComObject<Tip>, ITfContext) -> Result<()> + Send + 'static,
) -> Result<()> {
    use windows::Win32::System::LibraryLoader::LoadLibraryW;
    use windows::Win32::UI::Controls::EM_SETSEL;
    hidden_desktop(move || unsafe {
        let manager: ITfThreadMgr =
            CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
        let mut tid = 0;
        manager
            .cast::<ITfThreadMgrEx>()?
            .ActivateEx(&mut tid, TF_TMAE_NOACTIVATETIP)?;
        let library = LoadLibraryW(windows::core::w!("msftedit.dll"))?;
        let parent = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            windows::core::w!("STATIC"),
            None,
            WS_POPUP | WS_VISIBLE,
            0,
            0,
            320,
            80,
            None,
            None,
            None,
            None,
        )?;
        let edit = rich_edit_control(parent, windows::core::w!("AB"))?;
        let result = (|| {
            let _ = SetFocus(Some(edit));
            dispatch_pending();
            // RichEdit also exposes a CUAS document for this HWND. Its
            // focused document is the native TSF text store.
            let context = manager.GetFocus()?.GetTop()?;
            assert!(context.GetActiveView()?.GetWnd()? == edit);
            SendMessageW(edit, EM_SETSEL, Some(WPARAM(2)), Some(LPARAM(2)));
            SetKeyboardState(&[0; 256])?;
            let tip = test_tip(&manager)?;
            tip.shared.manager.replace(Some(manager.clone()));
            check_richedit_modules()?;
            check(parent, edit, tip, context)?;
            check_richedit_modules()
        })();
        DestroyWindow(parent)?;
        manager.Deactivate()?;
        FreeLibrary(library)?;
        result
    })
}

#[test]
#[ignore = "TSF RichEdit loads the system IME"]
fn edit_recovery_richedit_text_limit() -> Result<()> {
    use windows::Win32::UI::Controls::RichEdit::EM_EXLIMITTEXT;
    with_richedit(|_, edit, tip, context| unsafe {
        SendMessageW(edit, EM_EXLIMITTEXT, None, Some(LPARAM(3)));
        let server = serve_edits(&[Reply::Compose, Reply::Commit, Reply::Compose], Some(3))?;
        press(&tip, &context, starter()?)?;
        press(&tip, &context, 0x0d)?;
        wait_for_text(edit, "AB字");
        press(&tip, &context, starter()?)?;
        wait_for_text(edit, "AB字");
        assert!(tip.shared.composition.borrow().is_none());
        finish(server)?;
        SendMessageW(edit, EM_EXLIMITTEXT, None, Some(LPARAM(4)));
        let fresh = serve(&[Reply::Commit])?;
        press(&tip, &context, starter()?)?;
        wait_for_text(edit, "AB字字");
        tip.shared.client.borrow_mut().take();
        finish(fresh)
    })
}

thread_local! {
    static ON_CHANGE: Cell<Option<Callback>> = const { Cell::new(None) };
    static CHANGE_RESULT: Cell<Option<Result<()>>> = const { Cell::new(None) };
}

unsafe extern "system" fn rich_edit_notification(
    window: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    if message == WM_COMMAND
        && (w.0 >> 16) as u32 == EN_CHANGE
        && let Some(callback) = ON_CHANGE.take()
    {
        CHANGE_RESULT.set(Some(callback()));
    }
    unsafe { windows::Win32::UI::Shell::DefSubclassProc(window, message, w, l) }
}

#[test]
#[ignore = "TSF RichEdit loads the system IME"]
fn reentry_richedit_change_during_commit_and_preedit() -> Result<()> {
    use windows::Win32::UI::{Controls::RichEdit::*, Shell::SetWindowSubclass};
    with_richedit(|parent, edit, tip, context| unsafe {
        SetWindowSubclass(parent, Some(rich_edit_notification), 1, 0).ok()?;
        SendMessageW(
            edit,
            EM_SETEVENTMASK,
            None,
            Some(LPARAM(ENM_CHANGE as isize)),
        );
        let server = serve(&[Reply::Compose, Reply::CommitAndCompose, Reply::Commit])?;
        let vk = starter()?;
        press(&tip, &context, vk)?;
        let keys = tip.to_interface();
        let target = context.clone();
        ON_CHANGE.set(Some(Box::new(move || {
            if deliver(&keys, &target, 0x0d)? {
                Ok(())
            } else {
                Err(Error::new(
                    E_FAIL,
                    "the nested key was passed to the application",
                ))
            }
        })));
        press(&tip, &context, vk)?;
        CHANGE_RESULT
            .take()
            .ok_or_else(|| Error::new(E_FAIL, "RichEdit did not notify its parent"))??;
        assert_eq!(HELLOS.get(), 1);
        wait_for_text(edit, "AB字字");
        assert!(tip.shared.composition.borrow().is_none());
        tip.shared.client.borrow_mut().take();
        finish(server)?;

        // A second EN_CHANGE handler replaces the text and ends the new mark.
        let server = serve(&[Reply::Compose, Reply::CommitAndCompose])?;
        let vk = starter()?;
        press(&tip, &context, vk)?;
        let fresh = serve(&[Reply::Compose, Reply::Commit])?;
        let target = context.clone();
        let keys = tip.to_interface();
        ON_CHANGE.set(Some(Box::new(move || {
            deliver(&keys, &target, vk)?
                .then_some(())
                .ok_or_else(|| Error::new(E_FAIL, "starter passed before rewrite"))?;
            SetWindowTextW(edit, windows::core::w!("R"))?;
            deliver(&keys, &target, vk)?
                .then_some(())
                .ok_or_else(|| Error::new(E_FAIL, "starter passed after rewrite"))
        })));
        press(&tip, &context, vk)?;
        CHANGE_RESULT
            .take()
            .ok_or_else(|| Error::new(E_FAIL, "RichEdit did not notify its parent"))??;
        finish(server)?;
        // SetWindowText resets the insertion point to the beginning.
        wait_for_text(edit, "字R");
        tip.shared.client.borrow_mut().take();
        finish(fresh)?;

        SetWindowTextW(edit, windows::core::w!(""))?;
        let other = rich_edit_control(parent, windows::core::w!(""))?;
        let manager = tip.shared.manager.borrow().clone().ok_or_else(failure)?;
        let previous = context.GetDocumentMgr()?;
        let events: ITfThreadMgrEventSink = tip.to_interface();
        let keys = tip.to_interface();
        let server = serve(&[Reply::Compose, Reply::CommitAndCompose])?;
        press(&tip, &context, vk)?;
        // RichEdit terminates its mark during SetFocus. The unsubmitted
        // starter in the old control and the one in the new control each
        // need a clean conversion session.
        let mut listener = listener()?;
        let fresh = std::thread::spawn(move || {
            for step in [Reply::CommitConverted, Reply::Commit] {
                let pipe = accept_session(&mut listener)?;
                reply_to_session(pipe, [step], None, &[SpecialKey::None])?;
            }
            Ok(())
        });
        let old = context.clone();
        ON_CHANGE.set(Some(Box::new(move || {
            deliver(&keys, &old, vk)?
                .then_some(())
                .ok_or_else(failure)?;
            let _ = SetFocus(Some(other));
            check_richedit_modules()?;
            let focused = manager.GetFocus()?;
            let target = focused.GetTop()?;
            if target.GetActiveView()?.GetWnd()? != other {
                return Err(failure());
            }
            events.OnSetFocus(&focused, &previous)?;
            deliver(&keys, &target, vk)?
                .then_some(())
                .ok_or_else(|| Error::new(E_FAIL, "starter passed in the new RichEdit"))
        })));
        press(&tip, &context, vk)?;
        CHANGE_RESULT.take().ok_or_else(failure)??;
        finish(server)?;
        wait_for_text(other, "字");
        wait_for_text(edit, "字じ漢字");
        tip.shared.client.borrow_mut().take();
        finish(fresh)?;
        Ok(())
    })
}

#[test]
#[ignore = "TSF RichEdit loads the system IME"]
fn reentry_richedit_mode_switch_then_direct_input() -> Result<()> {
    use windows::Win32::UI::{
        Controls::{EM_REPLACESEL, RichEdit::*},
        Shell::SetWindowSubclass,
    };
    with_richedit(|parent, edit, tip, context| unsafe {
        SetWindowSubclass(parent, Some(rich_edit_notification), 1, 0).ok()?;
        SendMessageW(
            edit,
            EM_SETEVENTMASK,
            None,
            Some(LPARAM(ENM_CHANGE as isize)),
        );
        let manager = tip.shared.manager.borrow().clone().ok_or_else(failure)?;
        let mode = input_mode::Mode::attach(&manager, tip.shared.tid.get(), &tip.to_interface())?;
        let switch = mode.switch();
        let original = switch.japanese();
        tip.mode.replace(Some(mode));
        switch.apply(tip.shared.tid.get(), input_mode::Command::On)?;
        let server = serve(&[Reply::Compose, Reply::CommitAndCompose, Reply::Commit])?;
        let vk = starter()?;
        press(&tip, &context, vk)?;
        let keys = tip.to_interface();
        let target = context.clone();
        ON_CHANGE.set(Some(Box::new(move || {
            for key in [vk, 0x1a] {
                deliver(&keys, &target, key)?
                    .then_some(())
                    .ok_or_else(failure)?;
            }
            if !deliver(&keys, &target, vk)? {
                // RichEdit inserts passed characters at its current selection.
                SendMessageW(
                    edit,
                    EM_REPLACESEL,
                    None,
                    Some(LPARAM(windows::core::w!("x").as_ptr() as isize)),
                );
                return Err(Error::new(E_FAIL, "direct input overtook the mode switch"));
            }
            Ok(())
        })));
        press(&tip, &context, vk)?;
        let nested = CHANGE_RESULT.take().ok_or_else(failure)?;
        let direct = !switch.japanese();
        switch.apply(
            tip.shared.tid.get(),
            if original {
                input_mode::Command::On
            } else {
                input_mode::Command::Off
            },
        )?;
        nested?;
        assert!(direct);
        wait_for_text(edit, "AB字字");
        assert!(IsWindowVisible(LAST_HINT.get()).as_bool());
        finish(server)
    })
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
fn key_routing_connected_starter_passes_after_a_denied_read_on_down() -> Result<()> {
    with_host(|host| {
        let server = serve(&[])?;
        host.tip.shared.connect(false);
        let vk = starter()?;
        assert!(host.test(vk)?);
        host.store.busy.set(true);
        assert!(!host.down(vk)?);
        host.check_up(vk, false)?;
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
        host.tip.shared.show_mode_hint.set(false);
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
                    translate(WPARAM(vk), LPARAM(1))
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
