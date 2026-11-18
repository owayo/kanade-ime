//! Read the candidate status from the installed TIP while its real worker scores a list.
#![cfg(windows)]
mod common;
use std::time::{Duration, Instant};
use windows::{
    Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::{TextServices::*, WindowsAndMessaging::*},
    },
    core::{Interface, Result, w},
};

fn candidate_status(manager: &ITfUIElementMgr) -> Result<String> {
    unsafe {
        let elements = manager.EnumUIElements()?;
        let mut item = [None];
        let mut count = 0;
        loop {
            elements.Next(&mut item, &mut count)?;
            if count == 0 {
                return Ok(String::new());
            }
            if let Some(element) = item[0].take() {
                let label = element.GetDescription()?.to_string();
                if label.starts_with("Kanade 候補") {
                    return Ok(label);
                }
            }
        }
    }
}

#[test]
#[ignore = "requires the installed Kanade profile, learning and a qualified enabled Tsumugi model"]
fn candidate_list_reports_local_scoring() -> Result<()> {
    common::with_rich_edit(w!("劇団の東京"), |window, keys, context, _| unsafe {
        let on = keys.GetPreservedKey(
            context,
            &TF_PRESERVEDKEY {
                uVKey: 0x16,
                uModifiers: TF_MOD_IGNORE_ALL_MODIFIER,
            },
        )?;
        assert!(keys.SimulatePreservedKey(context, &on)?.as_bool());
        common::dispatch_pending();
        SendMessageW(
            window,
            0xb1, /* EM_SETSEL */
            Some(WPARAM(5)),
            Some(LPARAM(5)),
        );
        let press = |vk| -> Result<()> {
            assert!(keys.TestKeyDown(WPARAM(vk), LPARAM(1))?.as_bool());
            assert!(keys.KeyDown(WPARAM(vk), LPARAM(1))?.as_bool());
            if keys
                .TestKeyUp(WPARAM(vk), LPARAM(0xc0000001u32 as isize))?
                .as_bool()
            {
                let _ = keys.KeyUp(WPARAM(vk), LPARAM(0xc0000001u32 as isize))?;
            }
            Ok(())
        };
        let ui = keys.cast::<ITfUIElementMgr>()?;
        for &vk in b"KOUEN " {
            press(vk as usize)?;
        }
        assert!(candidate_status(&ui)?.ends_with("AI 評価中…"));
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            common::dispatch_pending();
            let status = candidate_status(&ui)?;
            if status.ends_with("AI 評価済み") {
                break;
            }
            assert!(
                status.ends_with("AI 評価中…") && Instant::now() < deadline,
                "unexpected candidate status {status}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        press(0x1b)?;
        Ok(())
    })
}
