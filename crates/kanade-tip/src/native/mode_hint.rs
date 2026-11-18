//! A short-lived Windows tracking tooltip. Only caret geometry leaves the edit session.
use super::*;
use windows::{
    Win32::{
        Graphics::Gdi::{
            ClientToScreen, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromRect,
        },
        System::Threading::GetCurrentThreadId,
        UI::{
            Controls::*,
            HiDpi::GetDpiForWindow,
            Shell::{DefSubclassProc, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    },
    core::{PWSTR, w},
};

#[implement(ITfEditSession)]
struct SelectionRect {
    context: ITfContext,
    rect: Rc<Cell<Option<RECT>>>,
    _guard: ModuleGuard,
}

impl ITfEditSession_Impl for SelectionRect_Impl {
    fn DoEditSession(&self, cookie: u32) -> Result<()> {
        let mut selection = [TF_SELECTION::default()];
        let mut count = 0;
        unsafe {
            self.context
                .GetSelection(cookie, TF_DEFAULT_SELECTION, &mut selection, &mut count)?;
            let selected = selection.first_mut().ok_or_else(failure)?;
            let range = std::mem::ManuallyDrop::take(&mut selected.range).ok_or_else(failure)?;
            let anchor = if selected.style.ase == TF_AE_START {
                TF_ANCHOR_START
            } else {
                TF_ANCHOR_END
            };
            range.Collapse(cookie, anchor)?;
            let view = self.context.GetActiveView()?;
            let mut rect = RECT::default();
            let mut clipped = BOOL::default();
            view.GetTextExt(cookie, &range, &mut rect, &mut clipped)?;
            if !clipped.as_bool() && rect.bottom > rect.top {
                self.rect.set(Some(rect));
            }
        }
        Ok(())
    }
}

fn win32_caret() -> Option<RECT> {
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        GetGUIThreadInfo(GetCurrentThreadId(), &mut info).ok()?;
        if info.hwndCaret.is_invalid() || info.rcCaret.bottom <= info.rcCaret.top {
            return None;
        }
        let mut top_left = POINT {
            x: info.rcCaret.left,
            y: info.rcCaret.top,
        };
        if !ClientToScreen(info.hwndCaret, &mut top_left).as_bool() {
            return None;
        }
        Some(RECT {
            left: top_left.x,
            top: top_left.y,
            right: top_left.x + (info.rcCaret.right - info.rcCaret.left),
            bottom: top_left.y + (info.rcCaret.bottom - info.rcCaret.top),
        })
    }
}

const TIMER: usize = 0x4b4d;
unsafe extern "system" fn tooltip_proc(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    unsafe {
        if msg == WM_TIMER && w.0 == TIMER {
            let _ = KillTimer(Some(hwnd), TIMER);
            let _ = ShowWindow(hwnd, SW_HIDE);
            return LRESULT(0);
        }
        if msg == WM_NCHITTEST {
            return LRESULT(HTTRANSPARENT as isize);
        }
        if msg == WM_MOUSEACTIVATE {
            return LRESULT(MA_NOACTIVATE as isize);
        }
        DefSubclassProc(hwnd, msg, w, l)
    }
}

pub(super) struct Hint(HWND);
impl Hint {
    pub(super) fn at_selection(context: &ITfContext, tid: u32, japanese: bool) -> Result<Self> {
        let result = Rc::new(Cell::new(None));
        let edit: ITfEditSession = SelectionRect {
            context: context.clone(),
            rect: result.clone(),
            _guard: ModuleGuard::new(),
        }
        .into();
        unsafe {
            let _ = context.RequestEditSession(tid, &edit, TF_ES_SYNC | TF_ES_READ);
            let caret = result.get().or_else(win32_caret).ok_or_else(failure)?;
            let owner = GetFocus();
            if owner.is_invalid() {
                return Err(failure());
            }
            InitCommonControlsEx(&INITCOMMONCONTROLSEX {
                dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_WIN95_CLASSES,
            })
            .ok()?;
            // Unowned so an editor destroying its child window cannot invalidate our handle.
            let hint = Self(CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT,
                TOOLTIPS_CLASSW,
                None,
                WS_POPUP | WINDOW_STYLE(TTS_ALWAYSTIP | TTS_NOPREFIX | TTS_NOANIMATE | TTS_NOFADE),
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )?);
            let hwnd = hint.0;
            SetWindowSubclass(hwnd, Some(tooltip_proc), TIMER, 0).ok()?;
            let label = if japanese {
                w!("あ  ひらがな")
            } else {
                w!("A  半角英数")
            };
            let info = TTTOOLINFOW {
                // Hosts without a v6 common-controls manifest accept the v2 layout.
                // The reserved trailing pointer is unused by tracking tools.
                cbSize: std::mem::offset_of!(TTTOOLINFOW, lpReserved) as u32,
                uFlags: TTF_TRACK | TTF_ABSOLUTE | TTF_TRANSPARENT,
                hwnd: owner,
                uId: 1,
                lpszText: PWSTR(label.as_ptr().cast_mut()),
                ..Default::default()
            };
            let tool = LPARAM((&raw const info) as isize);
            if SendMessageW(hwnd, TTM_ADDTOOLW, None, Some(tool)).0 == 0 {
                return Err(failure());
            }
            let gap = ((GetDpiForWindow(owner).max(96) * 5) / 96) as i32;
            let xy = (caret.left as u16 as u32) | (((caret.bottom + gap) as u16 as u32) << 16);
            SendMessageW(hwnd, TTM_TRACKPOSITION, None, Some(LPARAM(xy as isize)));
            SendMessageW(hwnd, TTM_TRACKACTIVATE, Some(WPARAM(1)), Some(tool));
            let mut bubble = RECT::default();
            GetWindowRect(hwnd, &mut bubble)?;
            let (width, height) = (bubble.right - bubble.left, bubble.bottom - bubble.top);
            let mut monitor = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            GetMonitorInfoW(
                MonitorFromRect(&caret, MONITOR_DEFAULTTONEAREST),
                &mut monitor,
            )
            .ok()?;
            let work = monitor.rcWork;
            let x = caret
                .left
                .clamp(work.left, (work.right - width).max(work.left));
            let y = if caret.bottom + gap + height <= work.bottom {
                caret.bottom + gap
            } else {
                caret.top - gap - height
            };
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                x,
                y.clamp(work.top, (work.bottom - height).max(work.top)),
                0,
                0,
                SWP_NOACTIVATE | SWP_NOSIZE,
            )?;
            if SetTimer(Some(hwnd), TIMER, 1000, None) == 0 {
                return Err(Error::from_thread());
            }
            Ok(hint)
        }
    }
}
impl Drop for Hint {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}
