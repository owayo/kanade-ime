//! Exercise caret updates through the installed Kanade DLL in a real text store.
#![cfg(windows)]
mod common;
use windows::{
    Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::{TextServices::*, WindowsAndMessaging::*},
    },
    core::{Result, w},
};

#[test]
#[ignore = "requires a registered Japanese Kanade profile and its running engine"]
fn consecutive_commits_in_rich_edit() -> Result<()> {
    common::with_rich_edit(w!("前後"), |window, keys, context, _| unsafe {
        let on = keys.GetPreservedKey(
            context,
            &TF_PRESERVEDKEY {
                uVKey: 0x16,
                uModifiers: TF_MOD_IGNORE_ALL_MODIFIER,
            },
        )?;
        assert!(keys.SimulatePreservedKey(context, &on)?.as_bool());
        common::dispatch_pending();
        // Start between two characters, so document end is not the expected caret.
        SendMessageW(
            window,
            0xb1, /* EM_SETSEL */
            Some(WPARAM(1)),
            Some(LPARAM(1)),
        );
        for (vk, expected, position) in [(0x41, "前ああああ後", 5), (0x49, "前ああああい後", 6)]
        {
            let count = if vk == 0x41 { 4 } else { 1 };
            for key in std::iter::repeat_n(vk, count).chain([0x0d]) {
                assert!(
                    keys.TestKeyDown(WPARAM(key), LPARAM(1))?.as_bool(),
                    "key was not accepted by Kanade"
                );
                assert!(keys.KeyDown(WPARAM(key), LPARAM(1))?.as_bool());
                if keys
                    .TestKeyUp(WPARAM(key), LPARAM(0xc0000001u32 as isize))?
                    .as_bool()
                {
                    let _ = keys.KeyUp(WPARAM(key), LPARAM(0xc0000001u32 as isize))?;
                }
            }
            let mut text = [0u16; 32];
            let n = GetWindowTextW(window, &mut text) as usize;
            assert_eq!(String::from_utf16_lossy(&text[..n]), expected);
            let positions = SendMessageW(window, 0xb0 /* EM_GETSEL */, None, None).0;
            assert_eq!((positions & 0xffff, positions >> 16), (position, position));
        }
        Ok(())
    })
}
