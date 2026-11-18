//! Installation diagnostics run in the signed IME, without starting a host session.
use anyhow::{Context, Result, ensure};
use objc2_foundation::{NSBundle, NSString, NSURL};
use std::ffi::c_void;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISRegisterInputSource(url: *const c_void) -> i32;
    fn TISCreateInputSourceList(properties: *const c_void, all: u8) -> *const c_void;
    fn TISGetInputSourceProperty(source: *const c_void, key: *const c_void) -> *const c_void;
    fn TISEnableInputSource(source: *const c_void) -> i32;
    fn TISCopyCurrentKeyboardInputSource() -> *const c_void;
    static kTISPropertyBundleID: *const c_void;
    static kTISPropertyInputSourceIsEnableCapable: *const c_void;
    static kTISPropertyInputSourceID: *const c_void;
    static kTISPropertyInputModeID: *const c_void;
    static kTISPropertyIconImageURL: *const c_void;
}
#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn LSRegisterURL(url: *const c_void, update: u8) -> i32;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: *const c_void);
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
    fn CFBooleanGetValue(value: *const c_void) -> u8;
    fn CFPreferencesAppSynchronize(application: *const c_void) -> u8;
}

#[derive(serde::Serialize)]
pub struct Status {
    pub installed: usize,
    pub enabled: usize,
    pub enabled_modes: Vec<String>,
    pub current: String,
    pub icon_files: Vec<String>,
}

fn enabled_input_sources(bundle_id: &NSString) -> Result<(usize, Vec<String>)> {
    // A bundle-filtered query can include modes which are absent from the
    // system's actual enabled list after a mode identifier changes.
    let sources = unsafe { TISCreateInputSourceList(std::ptr::null(), 0) };
    ensure!(!sources.is_null(), "enabled input sources unavailable");
    let mut enabled = 0;
    let mut modes = Vec::new();
    let expected = bundle_id.to_string();
    for index in 0..unsafe { CFArrayGetCount(sources) } {
        let source = unsafe { CFArrayGetValueAtIndex(sources, index) };
        let bundle = unsafe { TISGetInputSourceProperty(source, kTISPropertyBundleID) };
        if bundle.is_null() || unsafe { &*bundle.cast::<NSString>() }.to_string() != expected {
            continue;
        }
        enabled += 1;
        let mode = unsafe { TISGetInputSourceProperty(source, kTISPropertyInputModeID) };
        if !mode.is_null() {
            modes.push(unsafe { &*mode.cast::<NSString>() }.to_string());
        }
    }
    unsafe { CFRelease(sources) };
    Ok((enabled, modes))
}

fn current_source() -> Result<String> {
    let source = unsafe { TISCopyCurrentKeyboardInputSource() };
    ensure!(!source.is_null(), "current input source unavailable");
    let value = unsafe { TISGetInputSourceProperty(source, kTISPropertyInputSourceID) };
    // TIS specifies a CFString value; CFString and NSString are toll-free bridged.
    let id = if value.is_null() {
        String::new()
    } else {
        unsafe { &*value.cast::<NSString>() }.to_string()
    };
    unsafe { CFRelease(source) };
    ensure!(
        !id.is_empty(),
        "current input-source identifier unavailable"
    );
    Ok(id)
}

/// A status-only call never registers or enables an input source.
pub fn inspect(register: bool, enable: bool) -> Result<Status> {
    objc2::rc::autoreleasepool(|_| {
        let before = current_source()?;
        let bundle = NSBundle::mainBundle();
        let id = bundle
            .bundleIdentifier()
            .context("run the installed IME bundle")?;
        if enable {
            let (enabled, modes) = enabled_input_sources(&id)?;
            ensure!(
                enabled == 0 || !modes.is_empty(),
                "existing input mode registration is stale; remove and re-add Kanade in System Settings before enabling it"
            );
        }
        let url = bundle.bundleURL();
        let raw_url = (&*url as *const NSURL).cast::<c_void>();
        if register {
            let launch = unsafe { LSRegisterURL(raw_url, 1) };
            let tis = unsafe { TISRegisterInputSource(raw_url) };
            ensure!(
                launch == 0 && tis == 0,
                "input-source registration failed ({launch}, {tis})"
            );
        }
        let keys = [unsafe { kTISPropertyBundleID }];
        let values = [(&*id as *const NSString).cast::<c_void>()];
        let properties = unsafe {
            CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                1,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            )
        };
        ensure!(!properties.is_null(), "input-source properties unavailable");
        let sources = unsafe { TISCreateInputSourceList(properties, 1) };
        let mut installed = 0;
        let mut enable_status = 0;
        let mut icon_files = Vec::new();
        if !sources.is_null() {
            installed = unsafe { CFArrayGetCount(sources) } as usize;
            for index in 0..installed {
                let source = unsafe { CFArrayGetValueAtIndex(sources, index as isize) };
                let icon = unsafe { TISGetInputSourceProperty(source, kTISPropertyIconImageURL) };
                // TIS returns a CFURL, toll-free bridged to NSURL. Only the resource
                // filename belongs in diagnostics, never the installation path.
                if !icon.is_null()
                    && let Some(name) = unsafe { &*icon.cast::<NSURL>() }.lastPathComponent()
                {
                    let name = name.to_string();
                    if !icon_files.contains(&name) {
                        icon_files.push(name);
                    }
                }
            }
            if enable {
                for index in 0..installed {
                    let source = unsafe { CFArrayGetValueAtIndex(sources, index as isize) };
                    let capable = unsafe {
                        TISGetInputSourceProperty(source, kTISPropertyInputSourceIsEnableCapable)
                    };
                    if !capable.is_null() && unsafe { CFBooleanGetValue(capable) } != 0 {
                        let code = unsafe { TISEnableInputSource(source) };
                        if code != 0 {
                            enable_status = code;
                        }
                    }
                }
            }
            unsafe { CFRelease(sources) };
        }
        unsafe { CFRelease(properties) };
        let (enabled, enabled_modes) = enabled_input_sources(&id)?;
        ensure!(
            enable_status == 0,
            "input-source enabling failed ({enable_status})"
        );
        if enable {
            let domain = NSString::from_str("com.apple.HIToolbox");
            ensure!(
                unsafe { CFPreferencesAppSynchronize((&*domain as *const NSString).cast()) } != 0,
                "input-source preferences could not synchronize"
            );
        }
        let current = current_source()?;
        ensure!(
            before == current,
            "the selected input source changed during registration"
        );
        Ok(Status {
            installed,
            enabled,
            enabled_modes,
            current,
            icon_files,
        })
    })
}
