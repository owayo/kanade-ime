use super::*;
use std::sync::Mutex;
use windows::{
    Win32::{
        Graphics::{
            Direct2D::{Common::*, *},
            DirectWrite::*,
            Gdi::*,
        },
        UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
    },
    core::{PCWSTR, w},
};
static CLASS_USERS: Mutex<usize> = Mutex::new(0);
const CLASS: PCWSTR = w!("KanadeCandidateWindow");
pub struct CandidateWindow {
    hwnd: HWND,
    owner: HWND,
    state: Rc<Paint>,
}
struct Paint {
    _guard: ModuleGuard,
    page: RefCell<ipc::CandidatePage>,
    factory: ID2D1Factory,
    text: RefCell<IDWriteTextFormat>,
    footer: IDWriteTextFormat,
    fonts: IDWriteFactory,
    target: RefCell<Option<ID2D1HwndRenderTarget>>,
    select: Box<dyn Fn(ipc::CandidateHandle)>,
    poll: Box<dyn Fn()>,
    polls: Cell<u8>,
}
fn text_format(fonts: &IDWriteFactory, size: f32) -> Result<IDWriteTextFormat> {
    unsafe {
        let format = fonts.CreateTextFormat(
            w!("Yu Gothic UI"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!("ja-JP"),
        )?;
        format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        let sign = fonts.CreateEllipsisTrimmingSign(&format)?;
        format.SetTrimming(
            &DWRITE_TRIMMING {
                granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                ..Default::default()
            },
            &sign,
        )?;
        Ok(format)
    }
}
impl CandidateWindow {
    pub fn new(
        owner: HWND,
        select: impl Fn(ipc::CandidateHandle) + 'static,
        poll: impl Fn() + 'static,
    ) -> Result<Self> {
        let module = HINSTANCE(MODULE.load(Ordering::Acquire) as *mut c_void);
        let mut users = CLASS_USERS.lock().map_err(|_| failure())?;
        if *users == 0 {
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: module,
                lpszClassName: CLASS,
                ..Default::default()
            };
            if unsafe { RegisterClassW(&class) } == 0
                && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS
            {
                return Err(Error::from_thread());
            }
        }
        let factory =
            unsafe { D2D1CreateFactory::<ID2D1Factory>(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
        let fonts: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        let text = text_format(&fonts, 17.0)?;
        let footer = text_format(&fonts, 13.0)?;
        let state = Rc::new(Paint {
            _guard: ModuleGuard::new(),
            page: RefCell::new(ipc::CandidatePage::default()),
            factory,
            text: RefCell::new(text),
            footer,
            fonts,
            target: RefCell::new(None),
            select: Box::new(select),
            poll: Box::new(poll),
            polls: Cell::new(0),
        });
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                CLASS,
                w!(""),
                WS_POPUP,
                0,
                0,
                480,
                100,
                Some(owner),
                None,
                Some(module),
                Some(Rc::as_ptr(&state).cast()),
            )?
        };
        *users += 1;
        Ok(Self { hwnd, owner, state })
    }
    pub fn owned_by(&self, owner: HWND) -> bool {
        self.owner == owner && unsafe { IsWindow(Some(self.hwnd)) }.as_bool()
    }
    pub fn show(&mut self, page: &ipc::CandidatePage, caret: RECT) -> Result<()> {
        let old_size = self
            .state
            .page
            .try_borrow()
            .map_err(|_| failure())?
            .font_size;
        if old_size != page.font_size {
            let text = text_format(&self.state.fonts, page.font_size.clamp(12.0, 40.0))?;
            *self.state.text.try_borrow_mut().map_err(|_| failure())? = text;
        }
        if let Ok(mut current) = self.state.page.try_borrow_mut() {
            *current = page.clone();
        }
        let scale = unsafe { GetDpiForWindow(self.hwnd) }.max(96) as f64 / 96.0;
        let width = (520.0 * scale) as i32;
        let height = ((page.items.len() as f64
            * (f64::from(page.font_size.clamp(12.0, 40.0)) + 19.0)
            + 34.0)
            * scale) as i32;
        let mut x = caret.left;
        let mut y = caret.bottom + 4;
        let monitor = unsafe { MonitorFromRect(&caret, MONITOR_DEFAULTTONEAREST) };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            x = x
                .max(info.rcWork.left)
                .min((info.rcWork.right - width).max(info.rcWork.left));
            if y + height > info.rcWork.bottom {
                y = (caret.top - height - 4).max(info.rcWork.top);
            }
        }
        unsafe {
            SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )?;
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
        Ok(())
    }
    pub fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }
    pub fn stop_refinement(&self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), 1);
        }
    }
    pub fn schedule_refinement(&self, pending: bool) {
        self.stop_refinement();
        self.state.polls.set(0);
        if pending {
            // Window-owned timer messages cannot call an unloaded TIMERPROC.
            unsafe {
                SetTimer(Some(self.hwnd), 1, 60, None);
            }
        }
    }
}
impl Drop for CandidateWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
        if let Ok(mut users) = CLASS_USERS.lock() {
            *users = users.saturating_sub(1);
            if *users == 0 {
                unsafe {
                    let _ = UnregisterClassW(
                        CLASS,
                        Some(HINSTANCE(MODULE.load(Ordering::Acquire) as *mut c_void)),
                    );
                }
            }
        }
    }
}
unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let create = l.0 as *const CREATESTRUCTW;
        if !create.is_null() {
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*create).lpCreateParams as _);
            }
        }
    }
    let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Paint;
    match msg {
        WM_TIMER if w.0 == 1 && !state.is_null() => {
            // A refresh can synchronously replace/destroy the native window.
            // Hold its state and module reference until the callback returns.
            unsafe {
                Rc::increment_strong_count(state);
            }
            let state = unsafe { Rc::from_raw(state) };
            if state.polls.get() >= 10 {
                unsafe {
                    let _ = KillTimer(Some(hwnd), 1);
                }
            } else {
                state.polls.set(state.polls.get() + 1);
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (state.poll)()));
            }
            return LRESULT(0);
        }
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            unsafe {
                let _ = BeginPaint(hwnd, &mut ps);
            }
            if let Some(state) = unsafe { state.as_ref() } {
                let _ = state.draw(hwnd);
            }
            unsafe {
                let _ = EndPaint(hwnd, &ps);
            }
            return LRESULT(0);
        }
        WM_SIZE => {
            if let Some(state) = unsafe { state.as_ref() }
                && let Ok(mut target) = state.target.try_borrow_mut()
            {
                target.take();
            }
        }
        WM_LBUTTONUP => {
            if let Some(state) = unsafe { state.as_ref() } {
                let scale = unsafe { GetDpiForWindow(hwnd) }.max(96) as f64 / 96.0;
                let row_height = state
                    .page
                    .try_borrow()
                    .map(|p| f64::from(p.font_size.clamp(12.0, 40.0)) + 19.0)
                    .unwrap_or(36.0);
                let row = (((l.0 >> 16) & 0xffff) as f64 / scale / row_height) as usize;
                let handle = state
                    .page
                    .try_borrow()
                    .ok()
                    .and_then(|p| p.items.get(row).and_then(|c| c.handle));
                if let Some(handle) = handle {
                    (state.select)(handle);
                }
            }
            return LRESULT(0);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}
impl Paint {
    fn draw(&self, hwnd: HWND) -> Result<()> {
        let mut target = self.target.try_borrow_mut().map_err(|_| failure())?;
        if target.is_none() {
            let mut rect = RECT::default();
            unsafe {
                GetClientRect(hwnd, &mut rect)?;
            }
            let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32;
            *target = Some(unsafe {
                self.factory.CreateHwndRenderTarget(
                    &D2D1_RENDER_TARGET_PROPERTIES {
                        dpiX: dpi,
                        dpiY: dpi,
                        ..Default::default()
                    },
                    &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                        hwnd,
                        pixelSize: D2D_SIZE_U {
                            width: (rect.right - rect.left).max(1) as u32,
                            height: (rect.bottom - rect.top).max(1) as u32,
                        },
                        presentOptions: D2D1_PRESENT_OPTIONS_NONE,
                    },
                )?
            });
        }
        let rt = target.as_ref().ok_or_else(failure)?;
        let page = self.page.try_borrow().map_err(|_| failure())?;
        let text_format = self.text.try_borrow().map_err(|_| failure())?;
        let row_height = page.font_size.clamp(12.0, 40.0) + 19.0;
        let ink = unsafe {
            rt.CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    r: 0.12,
                    g: 0.15,
                    b: 0.2,
                    a: 1.0,
                },
                None,
            )?
        };
        let accent = unsafe {
            rt.CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    r: 0.9,
                    g: 0.94,
                    b: 1.0,
                    a: 1.0,
                },
                None,
            )?
        };
        unsafe {
            rt.BeginDraw();
            rt.Clear(Some(&D2D1_COLOR_F {
                r: 0.99,
                g: 0.99,
                b: 1.0,
                a: 1.0,
            }));
        }
        for (row, item) in page.items.iter().enumerate() {
            let top = row as f32 * row_height;
            let rect = D2D_RECT_F {
                left: 8.0,
                top,
                right: 512.0,
                bottom: top + row_height - 1.0,
            };
            if page.start + row as u32 == page.selected {
                unsafe {
                    rt.FillRectangle(&rect, &accent);
                }
            }
            let annotation = match item.annotation.as_str() {
                "ひらがな" | "カタカナ" | "アルファベット" => "",
                other => other,
            };
            let text: Vec<u16> = format!("{}  {}   {}", item.number, item.value, annotation)
                .encode_utf16()
                .collect();
            unsafe {
                rt.DrawText(
                    &text,
                    &*text_format,
                    &rect,
                    &ink,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP | D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
        }
        let footer: Vec<u16> = format!("{}/{}", page.selected + 1, page.total)
            .encode_utf16()
            .collect();
        let top = page.items.len() as f32 * row_height;
        unsafe {
            rt.DrawText(
                &footer,
                &self.footer,
                &D2D_RECT_F {
                    left: 10.0,
                    top,
                    right: 510.0,
                    bottom: top + 32.0,
                },
                &ink,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
        if unsafe { rt.EndDraw(None, None) }.is_err() {
            target.take();
        }
        Ok(())
    }
}
