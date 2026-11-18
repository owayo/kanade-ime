//! Verify half-width input through the installed TIP and Windows character translation.
#![cfg(windows)]
mod common;
use windows::{
    Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::{TextServices::*, WindowsAndMessaging::*},
    },
    core::{GUID, Result, w},
};

#[test]
#[ignore = "requires a registered Kanade Japanese input profile"]
fn installed_profile_direct_input() -> Result<()> {
    common::with_rich_edit(w!(""), |window, keys, context, open| unsafe {
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
        invoke(0x16)?;
        invoke(0x19)?;
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
        invoke(0xf4)?;
        assert_eq!(open.GetValue()?.Anonymous.Anonymous.Anonymous.lVal, 1);
        Ok(())
    })
}
