//! A short-lived Windows tracking tooltip. Only caret geometry leaves the edit session.
use super::*;
use std::time::Instant;
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

// Capture geometry while the key's field still has focus. A later Pop or
// focus change can destroy its window before the queue reports a discard.
pub(super) fn input_rect() -> RECT {
    win32_caret()
        .or_else(|| unsafe {
            let mut rect = RECT::default();
            GetWindowRect(GetFocus(), &mut rect).ok().map(|()| rect)
        })
        .unwrap_or(RECT {
            right: 1,
            bottom: 1,
            ..Default::default()
        }) // Primary monitor's origin.
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

pub(super) struct Hint {
    window: HWND,
    // The timer only hides the window. Replacing or dropping it ends the cooldown.
    pub(super) outage_shown_at: Option<Instant>,
}
impl Hint {
    pub(super) fn at_selection(context: &ITfContext, tid: u32, japanese: bool) -> Result<Self> {
        let label = if japanese {
            w!("あ  ひらがな")
        } else {
            w!("A  半角英数")
        };
        Self::with_label(context, tid, label, 2000)
    }
    pub(super) fn for_outage(context: &ITfContext, tid: u32) -> Result<Self> {
        let mut hint = Self::with_label(
            context,
            tid,
            w!("変換エンジンに接続できません。少し待って入力し直してください"),
            5000,
        )?;
        hint.outage_shown_at = Some(Instant::now());
        Ok(hint)
    }
    pub(super) fn for_discard(input: RECT) -> Result<Self> {
        Self::show_at(
            None,
            input,
            w!("Kanade はこの入力を反映できず、破棄しました"),
            5000,
        )
    }
    fn with_label(
        context: &ITfContext,
        tid: u32,
        label: windows::core::PCWSTR,
        duration_ms: u32,
    ) -> Result<Self> {
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
            Self::show_at(Some(owner), caret, label, duration_ms)
        }
    }
    fn show_at(
        focus_owner: Option<HWND>,
        anchor: RECT,
        label: windows::core::PCWSTR,
        duration_ms: u32,
    ) -> Result<Self> {
        unsafe {
            InitCommonControlsEx(&INITCOMMONCONTROLSEX {
                dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_WIN95_CLASSES,
            })
            .ok()?;
            let mut monitor = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            GetMonitorInfoW(
                MonitorFromRect(&anchor, MONITOR_DEFAULTTONEAREST),
                &mut monitor,
            )
            .ok()?;
            let work = monitor.rcWork;
            // Unowned so an editor destroying its child window cannot invalidate our handle.
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT,
                TOOLTIPS_CLASSW,
                None,
                WS_POPUP | WINDOW_STYLE(TTS_ALWAYSTIP | TTS_NOPREFIX | TTS_NOANIMATE | TTS_NOFADE),
                work.left,
                work.top,
                0,
                0,
                None,
                None,
                None,
                None,
            )?;
            let hint = Self {
                window: hwnd,
                outage_shown_at: None,
            };
            SetWindowSubclass(hwnd, Some(tooltip_proc), TIMER, 0).ok()?;
            // Only forward mouse messages when the tool has a distinct owner.
            let mut flags = TTF_TRACK | TTF_ABSOLUTE;
            if focus_owner.is_some() {
                flags |= TTF_TRANSPARENT;
            }
            let owner = focus_owner.unwrap_or(hwnd);
            let info = TTTOOLINFOW {
                // Hosts without a v6 common-controls manifest accept the v2 layout.
                // The reserved trailing pointer is unused by tracking tools.
                cbSize: std::mem::offset_of!(TTTOOLINFOW, lpReserved) as u32,
                uFlags: flags,
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
            let (x, y) = if focus_owner.is_some() {
                (anchor.left, anchor.bottom + gap)
            } else {
                (work.left, work.top)
            };
            let xy = (x as u16 as u32) | ((y as u16 as u32) << 16);
            SendMessageW(hwnd, TTM_TRACKPOSITION, None, Some(LPARAM(xy as isize)));
            SendMessageW(hwnd, TTM_TRACKACTIVATE, Some(WPARAM(1)), Some(tool));
            let mut bubble = RECT::default();
            GetWindowRect(hwnd, &mut bubble)?;
            let (width, height) = (bubble.right - bubble.left, bubble.bottom - bubble.top);
            let (x, y) = if focus_owner.is_none() {
                (work.right - width - gap, work.bottom - height - gap)
            } else if anchor.bottom + gap + height <= work.bottom {
                (anchor.left, anchor.bottom + gap)
            } else {
                (anchor.left, anchor.top - gap - height)
            };
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                x.clamp(work.left, (work.right - width).max(work.left)),
                y.clamp(work.top, (work.bottom - height).max(work.top)),
                0,
                0,
                SWP_NOACTIVATE | SWP_NOSIZE,
            )?;
            if !IsWindowVisible(hwnd).as_bool() {
                return Err(failure());
            }
            if SetTimer(Some(hwnd), TIMER, duration_ms, None) == 0 {
                return Err(Error::from_thread());
            }
            #[cfg(test)]
            host::hinted(hwnd);
            Ok(hint)
        }
    }
}
impl Drop for Hint {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.window);
        }
    }
}
