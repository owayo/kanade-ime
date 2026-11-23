use super::*;
use windows::{
    Win32::System::Registry::*,
    core::{PCWSTR, w},
};
const KEY: PCWSTR = w!("Software\\Classes\\CLSID\\{6DAFA84E-C56A-42BE-983E-0D18F3DC5B21}");
const SERVER: PCWSTR =
    w!("Software\\Classes\\CLSID\\{6DAFA84E-C56A-42BE-983E-0D18F3DC5B21}\\InprocServer32");
/// Position of the brand icon among the icon groups that `build.rs` links
/// into this DLL. Windows shows the icon in the input switcher and in the
/// taskbar's input indicator.
///
/// The profile is shared by both architectures and keeps the path of the DLL
/// registered last. The 64-bit and the 32-bit DLL carry the same icon at the
/// same position, so the order of registration does not change what is shown.
pub const BRAND_ICON_INDEX: u32 = 0;
struct RegistryKey(HKEY);
impl Drop for RegistryKey {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}
struct ComScope(bool);
impl ComScope {
    fn new() -> Result<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            return Err(hr.into());
        }
        Ok(Self(hr.is_ok()))
    }
}
impl Drop for ComScope {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                CoUninitialize();
            }
        }
    }
}
pub fn register() -> Result<()> {
    let _com = ComScope::new()?;
    let module = module_path()?;
    let mut path: Vec<u16> = module.to_string_lossy().encode_utf16().collect();
    path.push(0);
    let bytes: Vec<u8> = path.iter().flat_map(|c| c.to_le_bytes()).collect();
    let mut key = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            SERVER,
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
        .ok()?;
    }
    let key = RegistryKey(key);
    unsafe {
        RegSetValueExW(key.0, None, None, REG_SZ, Some(&bytes)).ok()?;
    }
    let apartment: Vec<u8> = "Apartment\0"
        .encode_utf16()
        .flat_map(|c| c.to_le_bytes())
        .collect();
    unsafe {
        RegSetValueExW(key.0, w!("ThreadingModel"), None, REG_SZ, Some(&apartment)).ok()?;
    }
    let profiles: ITfInputProcessorProfiles =
        unsafe { CoCreateInstance(&CLSID_TF_InputProcessorProfiles, None, CLSCTX_INPROC_SERVER)? };
    unsafe {
        profiles.Register(&CLSID)?;
        profiles.AddLanguageProfile(
            &CLSID,
            0x0411,
            &PROFILE,
            &"Kanade".encode_utf16().collect::<Vec<_>>(),
            &path,
            BRAND_ICON_INDEX,
        )?;
        profiles.EnableLanguageProfile(&CLSID, 0x0411, &PROFILE, true)?;
    }
    let categories: ITfCategoryMgr =
        unsafe { CoCreateInstance(&CLSID_TF_CategoryMgr, None, CLSCTX_INPROC_SERVER)? };
    for category in [
        GUID_TFCAT_TIP_KEYBOARD,
        GUID_TFCAT_DISPLAYATTRIBUTEPROVIDER,
        GUID_TFCAT_TIPCAP_COMLESS,
        GUID_TFCAT_TIPCAP_INPUTMODECOMPARTMENT,
        GUID_TFCAT_TIPCAP_UIELEMENTENABLED,
        GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT,
        GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT,
    ] {
        unsafe {
            categories.RegisterCategory(&CLSID, &category, &CLSID)?;
        }
    }
    Ok(())
}
pub fn unregister() -> Result<()> {
    let _com = ComScope::new()?;
    let mut failure = None;
    // TSF registrations are shared by both architectures; COM classes are not.
    // Check actual membership instead of hiding every E_FAIL from a second removal.
    let categories: Result<ITfCategoryMgr> =
        unsafe { CoCreateInstance(&CLSID_TF_CategoryMgr, None, CLSCTX_INPROC_SERVER) };
    match categories {
        Ok(categories) => {
            for category in [
                GUID_TFCAT_TIP_KEYBOARD,
                GUID_TFCAT_DISPLAYATTRIBUTEPROVIDER,
                GUID_TFCAT_TIPCAP_COMLESS,
                GUID_TFCAT_TIPCAP_INPUTMODECOMPARTMENT,
                GUID_TFCAT_TIPCAP_UIELEMENTENABLED,
                GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT,
                GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT,
            ] {
                let removed = (|| unsafe {
                    if contains_class(categories.EnumItemsInCategory(&category)?)? {
                        categories.UnregisterCategory(&CLSID, &category, &CLSID)?;
                    }
                    Ok(())
                })();
                if let Err(error) = removed {
                    failure.get_or_insert(error);
                }
            }
        }
        Err(error) => {
            failure.get_or_insert(error);
        }
    }
    let removed = (|| unsafe {
        let profiles: ITfInputProcessorProfiles =
            CoCreateInstance(&CLSID_TF_InputProcessorProfiles, None, CLSCTX_INPROC_SERVER)?;
        if contains_class(profiles.EnumInputProcessorInfo()?)? {
            profiles.Unregister(&CLSID)?;
        }
        Ok(())
    })();
    if let Err(error) = removed {
        failure.get_or_insert(error);
    }
    unsafe {
        let status = RegDeleteTreeW(HKEY_LOCAL_MACHINE, KEY);
        if status != ERROR_FILE_NOT_FOUND
            && let Err(error) = status.ok()
        {
            failure.get_or_insert(error);
        }
    }
    failure.map_or(Ok(()), Err)
}

fn contains_class(entries: IEnumGUID) -> Result<bool> {
    loop {
        let mut values = [GUID::zeroed()];
        let mut fetched = 0;
        unsafe { entries.Next(&mut values, Some(&mut fetched)).ok()? };
        if fetched == 0 {
            return Ok(false);
        }
        if values.first() == Some(&CLSID) {
            return Ok(true);
        }
    }
}
