//! Exercise caret updates in a real Rich Edit text store through the installed TIP.
#![cfg(windows)]
use std::time::{Duration, Instant};
use windows::{
    Win32::{
        Foundation::{LPARAM, WPARAM},
        System::{
            Com::*,
            LibraryLoader::LoadLibraryW,
            Ole::{OleInitialize, OleUninitialize},
        },
        UI::{
            Input::KeyboardAndMouse::{HKL, SetFocus},
            TextServices::*,
            WindowsAndMessaging::*,
        },
    },
    core::{GUID, Interface, Result, w},
};

#[test]
#[ignore = "requires a registered Japanese Kanade profile and its running engine"]
fn consecutive_commits_in_rich_edit() -> Result<()> {
    const TIP: GUID = GUID::from_u128(0x6dafa84e_c56a_42be_983e_0d18f3dc5b21);
    const PROFILE: GUID = GUID::from_u128(0xa1c81465_d31b_41d4_b7c4_35641955f560);
    unsafe { OleInitialize(None)? };
    let result = (|| unsafe {
        let manager: ITfThreadMgr =
            CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
        let tid = manager.Activate()?;
        let profiles: ITfInputProcessorProfileMgr =
            CoCreateInstance(&CLSID_TF_InputProcessorProfiles, None, CLSCTX_INPROC_SERVER)?;
        profiles.ActivateProfile(
            TF_PROFILETYPE_INPUTPROCESSOR,
            0x0411,
            &TIP,
            &PROFILE,
            HKL::default(),
            TF_IPPMF_FORPROCESS,
        )?;
        LoadLibraryW(w!("msftedit.dll"))?;
        let window = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("RICHEDIT50W"),
            w!("前後"),
            WS_POPUP | WS_VISIBLE,
            -30000,
            -30000,
            250,
            50,
            None,
            None,
            None,
            None,
        )?;
        // EM_SETEDITSTYLE with SES_USECTF enables the control's own TSF text store.
        SendMessageW(
            window,
            WM_USER + 204,
            Some(WPARAM(0x10000)),
            Some(LPARAM(0x10000)),
        );
        let _ = SetFocus(Some(window));
        let keys = manager.cast::<ITfKeystrokeMgr>()?;
        let start = Instant::now();
        loop {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&msg);
            }
            if keys.GetForeground().ok() == Some(TIP)
                && manager.GetFocus().and_then(|doc| doc.GetTop()).is_ok()
            {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "Rich Edit did not acquire the Kanade profile"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let slots = manager.cast::<ITfCompartmentMgr>()?;
        let open = slots.GetCompartment(&GUID_COMPARTMENT_KEYBOARD_OPENCLOSE)?;
        let conversion = slots.GetCompartment(&GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION)?;
        let old_open = open.GetValue()?;
        let old_conversion = conversion.GetValue()?;
        let check = (|| -> Result<()> {
            let context = manager.GetFocus()?.GetTop()?;
            let on = keys.GetPreservedKey(
                &context,
                &TF_PRESERVEDKEY {
                    uVKey: 0x16,
                    uModifiers: TF_MOD_IGNORE_ALL_MODIFIER,
                },
            )?;
            assert!(keys.SimulatePreservedKey(&context, &on)?.as_bool());
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
        })();
        open.SetValue(tid, &old_open)?;
        conversion.SetValue(tid, &old_conversion)?;
        DestroyWindow(window)?;
        manager.Deactivate()?;
        check
    })();
    unsafe { OleUninitialize() };
    result
}
