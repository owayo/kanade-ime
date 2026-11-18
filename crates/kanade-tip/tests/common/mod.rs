//! A disposable Rich Edit window with its own TSF text store and Kanade profile.
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
    core::{GUID, Interface, PCWSTR, Result, w},
};

pub fn dispatch_pending() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            DispatchMessageW(&msg);
        }
    }
}

pub fn with_rich_edit(
    initial: PCWSTR,
    exercise: impl FnOnce(
        windows::Win32::Foundation::HWND,
        &ITfKeystrokeMgr,
        &ITfContext,
        &ITfCompartment,
    ) -> Result<()>,
) -> Result<()> {
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
            initial,
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
            dispatch_pending();
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
        let context = manager.GetFocus()?.GetTop()?;
        let mut keyboard = [0; 256];
        windows::Win32::UI::Input::KeyboardAndMouse::GetKeyboardState(&mut keyboard)?;
        windows::Win32::UI::Input::KeyboardAndMouse::SetKeyboardState(&[0; 256])?;
        let check = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            exercise(window, &keys, &context, &open)
        }));
        windows::Win32::UI::Input::KeyboardAndMouse::SetKeyboardState(&keyboard)?;
        open.SetValue(tid, &old_open)?;
        conversion.SetValue(tid, &old_conversion)?;
        DestroyWindow(window)?;
        manager.Deactivate()?;
        match check {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })();
    unsafe { OleUninitialize() };
    result
}
