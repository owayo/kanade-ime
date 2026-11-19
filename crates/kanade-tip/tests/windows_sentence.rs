//! Check the first conversion in disposable TSF text stores.
#![cfg(windows)]
mod common;
use std::time::{Duration, Instant};
use windows::{
    Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::{
            TextServices::*,
            WindowsAndMessaging::{GetWindowTextW, SendMessageW},
        },
    },
    core::{Result, w},
};

#[test]
#[ignore = "requires a registered Kanade profile with its qualified model"]
fn first_conversion_uses_the_reading_and_other_segments() -> Result<()> {
    let mut mismatches = Vec::new();
    for (initial, prefix, romaji, converted) in [
        (w!(""), "", "KURUMANINORIMASU", "車に乗ります"),
        (w!(""), "", "ZASSHININORIMASU", "雑誌に載ります"),
        (w!(""), "", "KURUMANINORIMASU.", "車に乗ります。"),
        (w!(""), "", "ZASSHININORIMASU.", "雑誌に載ります。"),
        (w!(""), "", "NISENNKYUUNENNNOHARU", "2009年の春"),
        (
            w!("それは、梅の"),
            "それは、梅の",
            "KONOSHITANITATTEIRU",
            "木の下に立っている",
        ),
    ] {
        let expected = format!("{prefix}{converted}");
        // Escape after observing the preview so the fixture never trains personal history.
        common::with_rich_edit(initial, |window, keys, context, _| unsafe {
            // Place the insertion point after the committed context (EM_SETSEL).
            let end = prefix.encode_utf16().count();
            SendMessageW(
                window,
                0x00b1,
                Some(WPARAM(end)),
                Some(LPARAM(end as isize)),
            );
            common::dispatch_pending();
            let on = keys.GetPreservedKey(
                context,
                &TF_PRESERVEDKEY {
                    uVKey: 0x16,
                    uModifiers: TF_MOD_IGNORE_ALL_MODIFIER,
                },
            )?;
            assert!(keys.SimulatePreservedKey(context, &on)?.as_bool());
            common::dispatch_pending();
            let press = |vk: u8| -> Result<()> {
                let key = WPARAM(usize::from(vk));
                assert!(keys.TestKeyDown(key, LPARAM(1))?.as_bool());
                assert!(keys.KeyDown(key, LPARAM(1))?.as_bool());
                let up = LPARAM(0xc0000001u32 as isize);
                if keys.TestKeyUp(key, up)?.as_bool() {
                    let _ = keys.KeyUp(key, up)?;
                }
                common::dispatch_pending();
                Ok(())
            };
            for vk in romaji.bytes().chain(std::iter::once(b' ')) {
                press(if vk == b'.' { 0xbe } else { vk })?;
            }
            let deadline = Instant::now() + Duration::from_secs(1);
            let observed = loop {
                common::dispatch_pending();
                let mut text = [0u16; 128];
                let len = GetWindowTextW(window, &mut text) as usize;
                let value = String::from_utf16_lossy(&text[..len]);
                if value == expected || Instant::now() >= deadline {
                    break value;
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            press(0x1b)?;
            if observed != expected {
                mismatches.push((observed, expected));
            }
            Ok(())
        })?;
    }
    assert!(mismatches.is_empty(), "{mismatches:?}");
    Ok(())
}
