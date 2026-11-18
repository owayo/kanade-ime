//! Verify half-width input through the installed TIP and Windows character translation.
#![cfg(windows)]
mod common;
use std::time::{Duration, Instant};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, RECT, WPARAM},
        UI::{
            Controls::TOOLTIPS_CLASSW, Input::KeyboardAndMouse::GetFocus, TextServices::*,
            WindowsAndMessaging::*,
        },
    },
    core::{GUID, Result, w},
};

/// Find the visible tracking tooltip on the editor's own UI thread.
fn visible_hint(editor: HWND) -> Option<(HWND, String)> {
    let mut last = None;
    while let Ok(hwnd) = unsafe { FindWindowExW(None, last, TOOLTIPS_CLASSW, None) } {
        last = Some(hwnd);
        unsafe {
            if GetWindowThreadProcessId(hwnd, None) != GetWindowThreadProcessId(editor, None)
                || !IsWindowVisible(hwnd).as_bool()
            {
                continue;
            }
            let mut text = [0; 64];
            let len = GetWindowTextW(hwnd, &mut text);
            return Some((hwnd, String::from_utf16_lossy(&text[..len as usize])));
        }
    }
    None
}

#[test]
#[ignore = "requires a registered Kanade Japanese input profile"]
fn installed_profile_direct_input() -> Result<()> {
    common::with_rich_edit(w!(""), |window, keys, context, open| unsafe {
        SetWindowPos(
            window,
            None,
            160,
            120,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )?;
        let invoke = |vk| -> Result<()> {
            let id = keys.GetPreservedKey(
                context,
                &TF_PRESERVEDKEY {
                    uVKey: vk,
                    uModifiers: TF_MOD_IGNORE_ALL_MODIFIER,
                },
            )?;
            assert_ne!(id, GUID::zeroed());
            assert!(keys.SimulatePreservedKey(context, &id)?.as_bool());
            common::dispatch_pending();
            Ok(())
        };
        let check_hint = |label| -> Result<()> {
            let (hwnd, text) = visible_hint(window).expect("visible mode tooltip");
            assert_eq!(text, label);
            assert_eq!(GetFocus(), window);
            let mut bubble = RECT::default();
            GetWindowRect(hwnd, &mut bubble)?;
            let mut editor = RECT::default();
            GetWindowRect(window, &mut editor)?;
            assert!((editor.left - 10..=editor.right).contains(&bubble.left));
            assert!((editor.top..=editor.bottom + 20).contains(&bubble.top));
            Ok(())
        };
        invoke(0x16)?;
        check_hint("あ  ひらがな")?;
        invoke(0x19)?;
        check_hint("A  半角英数")?;
        assert_eq!(open.GetValue()?.Anonymous.Anonymous.Anonymous.lVal, 0);
        for vk in [0x41, 0x31, 0xba, 0x20, 0x0d, 0x08] {
            assert!(!keys.TestKeyDown(WPARAM(vk), LPARAM(0))?.as_bool());
        }
        for vk in [0x41, 0x42, 0x43, 0x31, 0x32, 0x33] {
            assert!(!keys.TestKeyDown(WPARAM(vk), LPARAM(1))?.as_bool());
            let press = MSG {
                hwnd: window,
                message: WM_KEYDOWN,
                wParam: WPARAM(vk),
                lParam: LPARAM(1),
                ..Default::default()
            };
            let _ = TranslateMessage(&press);
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, Some(window), WM_CHAR, WM_CHAR, PM_REMOVE).as_bool() {
                DispatchMessageW(&msg);
            }
        }
        let mut text = [0u16; 16];
        let n = GetWindowTextW(window, &mut text) as usize;
        assert_eq!(String::from_utf16_lossy(&text[..n]), "abc123");
        let start = Instant::now();
        while visible_hint(window).is_some() {
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "mode tooltip stayed visible"
            );
            std::thread::sleep(Duration::from_millis(15));
            common::dispatch_pending();
        }
        invoke(0xf4)?;
        check_hint("あ  ひらがな")?;
        assert_eq!(open.GetValue()?.Anonymous.Anonymous.Anonymous.lVal, 1);
        Ok(())
    })
}
