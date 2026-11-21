//! The built DLL carries the brand icon at the position that registration
//! hands to Windows. Nothing is registered: the file is only read.
#![cfg(windows)]
use std::{os::windows::ffi::OsStrExt, path::PathBuf};
use windows::{
    Win32::{
        Foundation::{FreeLibrary, HMODULE},
        System::LibraryLoader::{
            EnumResourceNamesW, FindResourceW, LOAD_LIBRARY_AS_DATAFILE,
            LOAD_LIBRARY_AS_IMAGE_RESOURCE, LoadLibraryExW, LoadResource, LockResource,
            SizeofResource,
        },
        UI::WindowsAndMessaging::{
            DestroyIcon, HICON, PrivateExtractIconsW, RT_GROUP_ICON, RT_ICON,
        },
    },
    core::{BOOL, HSTRING, PCWSTR},
};

const TEMPLATE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/icons/input-template.png"
);

/// Cargo builds the DLL into the directory of this test's executable.
fn dll() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    path.join("kanade_tip.dll")
}

unsafe extern "system" fn collect(_: HMODULE, _: PCWSTR, name: PCWSTR, names: isize) -> BOOL {
    // A numbered resource passes its number where a named one passes a pointer.
    unsafe { (*(names as *mut Vec<usize>)).push(name.0 as usize) };
    true.into()
}

fn resource(module: HMODULE, kind: PCWSTR, id: u16) -> Vec<u8> {
    unsafe {
        let found = FindResourceW(Some(module), PCWSTR(id as usize as *const u16), kind);
        assert!(!found.0.is_null(), "resource {id} is missing");
        let data = LoadResource(Some(module), found).unwrap();
        let bytes = LockResource(data).cast::<u8>();
        std::slice::from_raw_parts(bytes, SizeofResource(Some(module), found) as usize).to_vec()
    }
}

#[test]
fn built_dll_carries_the_brand_icon_registration_points_to() {
    let path = dll();
    assert!(path.is_file(), "{} was not built", path.display());
    let images = kanade_winres::brand_icon::render(&std::fs::read(TEMPLATE).unwrap()).unwrap();
    let module = unsafe {
        LoadLibraryExW(
            &HSTRING::from(path.as_os_str()),
            None,
            LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE,
        )
    }
    .unwrap();

    // Windows counts icon groups in this order when a position names an icon.
    let mut groups: Vec<usize> = Vec::new();
    unsafe {
        EnumResourceNamesW(
            Some(module),
            RT_GROUP_ICON,
            Some(collect),
            &mut groups as *mut Vec<usize> as isize,
        )
    }
    .unwrap();
    let group = *groups
        .get(kanade_tip::BRAND_ICON_INDEX as usize)
        .expect("no icon group at the registered position");
    let directory = resource(module, RT_GROUP_ICON, u16::try_from(group).unwrap());

    // Each directory entry names the bitmap resource of one size.
    assert_eq!(directory.len(), 6 + 14 * images.len());
    let ids: Vec<u16> = directory[6..]
        .as_chunks::<14>()
        .0
        .iter()
        .map(|entry| u16::from_le_bytes([entry[12], entry[13]]))
        .collect();
    assert_eq!(
        directory,
        kanade_winres::icon::group(&images, ids[0]).unwrap(),
        "the directory describes other images than the rendered icon"
    );
    for (image, &id) in images.iter().zip(&ids) {
        assert_eq!(
            resource(module, RT_ICON, id),
            image.bitmap(),
            "{} pixel bitmap",
            image.size()
        );
    }

    // The lookup Windows performs: file, position and wanted size.
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    let mut file = [0u16; 260];
    assert!(wide.len() < file.len(), "path is too long for the lookup");
    file[..wide.len()].copy_from_slice(&wide);
    for (image, &id) in images.iter().zip(&ids) {
        let size = image.size() as i32;
        let mut icon = [HICON::default()];
        let mut chosen = 0;
        let found = unsafe {
            PrivateExtractIconsW(
                &file,
                kanade_tip::BRAND_ICON_INDEX as i32,
                size,
                size,
                Some(&mut icon),
                Some(&mut chosen),
                0,
            )
        };
        assert_eq!(found, 1, "{size} pixel lookup");
        assert_eq!(
            chosen,
            u32::from(id),
            "{size} pixel lookup chose another size"
        );
        unsafe { DestroyIcon(icon[0]) }.unwrap();
    }
    unsafe { FreeLibrary(module) }.unwrap();
}
