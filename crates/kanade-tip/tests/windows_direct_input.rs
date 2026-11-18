//! Exercise the installed DLL through TSF in an isolated thread and document.
#![cfg(windows)]
use windows::{
    Win32::{
        Foundation::{LPARAM, WPARAM},
        System::Com::*,
        UI::{Input::KeyboardAndMouse::HKL, TextServices::*, WindowsAndMessaging::*},
    },
    core::{GUID, Interface, Result},
};

#[test]
#[ignore = "requires a registered Kanade Japanese input profile"]
fn installed_profile_direct_input() -> Result<()> {
    const TIP: GUID = GUID::from_u128(0x6dafa84e_c56a_42be_983e_0d18f3dc5b21);
    const PROFILE: GUID = GUID::from_u128(0xa1c81465_d31b_41d4_b7c4_35641955f560);
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
    let result = (|| unsafe {
        let thread: ITfThreadMgr =
            CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
        let window = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            windows::core::w!("EDIT"),
            windows::core::w!(""),
            WS_POPUP | WS_VISIBLE,
            -30000,
            -30000,
            100,
            30,
            None,
            None,
            None,
            None,
        )?;
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(window));
        let tid = thread.Activate()?;
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
        let doc = thread.CreateDocumentMgr()?;
        let mut context = None;
        let mut cookie = 0;
        doc.CreateContext(tid, 0, None, &mut context, &mut cookie)?;
        let context = context.ok_or(windows::Win32::Foundation::E_FAIL)?;
        doc.Push(&context)?;
        let _ = thread.AssociateFocus(window, &doc)?;
        thread.SetFocus(&doc)?;
        let mut active = TF_INPUTPROCESSORPROFILE::default();
        profiles.GetActiveProfile(&GUID_TFCAT_TIP_KEYBOARD, &mut active)?;
        assert_eq!(active.clsid, TIP);
        assert_eq!(active.guidProfile, PROFILE);
        let keys = thread.cast::<ITfKeystrokeMgr>()?;
        let start = std::time::Instant::now();
        while keys.GetForeground().ok() != Some(TIP)
            && start.elapsed() < std::time::Duration::from_secs(3)
        {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(keys.GetForeground()?, TIP);
        let compartments = thread.cast::<ITfCompartmentMgr>()?;
        let open = compartments.GetCompartment(&GUID_COMPARTMENT_KEYBOARD_OPENCLOSE)?;
        let old = open.GetValue()?;
        let invoke = |vk| -> Result<()> {
            let id = keys.GetPreservedKey(
                &context,
                &TF_PRESERVEDKEY {
                    uVKey: vk,
                    uModifiers: TF_MOD_IGNORE_ALL_MODIFIER,
                },
            )?;
            assert_ne!(id, GUID::zeroed());
            assert!(keys.SimulatePreservedKey(&context, &id)?.as_bool());
            Ok(())
        };
        invoke(0x16)?;
        invoke(0x19)?;
        assert_eq!(open.GetValue()?.Anonymous.Anonymous.Anonymous.lVal, 0);
        for vk in [0x41, 0x31, 0xba, 0x20, 0x0d, 0x08] {
            assert!(!keys.TestKeyDown(WPARAM(vk), LPARAM(0))?.as_bool());
        }
        let mut state = [0u8; 256];
        windows::Win32::UI::Input::KeyboardAndMouse::GetKeyboardState(&mut state)?;
        windows::Win32::UI::Input::KeyboardAndMouse::SetKeyboardState(&[0; 256])?;
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
        windows::Win32::UI::Input::KeyboardAndMouse::SetKeyboardState(&state)?;
        let mut text = [0u16; 16];
        let n = GetWindowTextW(window, &mut text) as usize;
        assert_eq!(String::from_utf16_lossy(&text[..n]), "abc123");
        invoke(0xf4)?;
        assert_eq!(open.GetValue()?.Anonymous.Anonymous.Anonymous.lVal, 1);
        open.SetValue(tid, &old)?;
        doc.Pop(TF_POPF_ALL)?;
        DestroyWindow(window)?;
        thread.Deactivate()
    })();
    unsafe { CoUninitialize() };
    result
}
