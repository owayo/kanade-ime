use super::*;
#[cfg(target_os = "macos")]
#[path = "install/input_source.rs"]
mod input_source;
#[cfg(target_os = "macos")]
pub(super) fn unregister_build_copy(path: &Path) -> Result<()> {
    if !path.is_dir() {
        return Ok(());
    }
    let result = Command::new("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister")
        .arg("-u").arg(path).output()?;
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    // kLSApplicationNotFoundErr is expected for a copy that was never registered.
    ensure!(
        result.status.success() || message.contains("-10814"),
        "build-copy unregistration failed"
    );
    Ok(())
}
#[cfg(target_os = "macos")]
fn discover_input_source() -> Result<serde_json::Value> {
    use objc2_foundation::NSString;
    use std::ffi::c_void;
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn TISCreateInputSourceList(properties: *const c_void, all: u8) -> *const c_void;
        fn TISGetInputSourceProperty(source: *const c_void, key: *const c_void) -> *const c_void;
        static kTISPropertyBundleID: *const c_void;
        static kTISPropertyInputModeID: *const c_void;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
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
        fn CFRelease(value: *const c_void);
    }
    let id = NSString::from_str("dev.kanade.inputmethod.Kanade");
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
    let count = |all| {
        let array = unsafe { TISCreateInputSourceList(properties, u8::from(all)) };
        if array.is_null() {
            return 0;
        }
        let count = unsafe { CFArrayGetCount(array) };
        unsafe { CFRelease(array) };
        count
    };
    let installed = count(true);
    unsafe { CFRelease(properties) };
    let sources = unsafe { TISCreateInputSourceList(std::ptr::null(), 0) };
    ensure!(!sources.is_null(), "enabled input sources unavailable");
    let mut enabled = 0;
    let mut enabled_modes = Vec::new();
    for index in 0..unsafe { CFArrayGetCount(sources) } {
        let source = unsafe { CFArrayGetValueAtIndex(sources, index) };
        let bundle = unsafe { TISGetInputSourceProperty(source, kTISPropertyBundleID) };
        if bundle.is_null() || unsafe { &*bundle.cast::<NSString>() }.to_string() != id.to_string()
        {
            continue;
        }
        enabled += 1;
        let mode = unsafe { TISGetInputSourceProperty(source, kTISPropertyInputModeID) };
        if !mode.is_null() {
            enabled_modes.push(unsafe { &*mode.cast::<NSString>() }.to_string());
        }
    }
    unsafe { CFRelease(sources) };
    Ok(
        serde_json::json!({"installed":installed,"enabled":enabled,"enabled_modes":enabled_modes,"current":current_input_source()?}),
    )
}
#[cfg(target_os = "macos")]
fn current_input_source() -> Result<String> {
    use std::ffi::c_void;
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn TISCopyCurrentKeyboardInputSource() -> *const c_void;
        fn TISGetInputSourceProperty(source: *const c_void, key: *const c_void) -> *const c_void;
        static kTISPropertyInputSourceID: *const c_void;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(value: *const c_void);
        fn CFStringGetCString(
            value: *const c_void,
            buffer: *mut u8,
            size: isize,
            encoding: u32,
        ) -> u8;
    }
    let source = unsafe { TISCopyCurrentKeyboardInputSource() };
    ensure!(
        !source.is_null(),
        "cannot determine the current input source"
    );
    let id = unsafe { TISGetInputSourceProperty(source, kTISPropertyInputSourceID) };
    let mut text = [0u8; 512];
    let valid =
        !id.is_null() && unsafe { CFStringGetCString(id, text.as_mut_ptr(), 512, 0x08000100) } != 0;
    unsafe { CFRelease(source) };
    ensure!(valid, "cannot determine the current input source");
    let end = text.iter().position(|c| *c == 0).unwrap_or(0);
    ensure!(end > 0, "cannot determine the current input source");
    Ok(String::from_utf8(text[..end].to_vec())?)
}
#[cfg(target_os = "macos")]
fn stop_previous_frontends(source: &Path, target: &Path, previous: &Path) -> Result<()> {
    use objc2_app_kit::NSRunningApplication;
    use objc2_foundation::{NSDate, NSRunLoop, NSString};
    use std::time::{Duration, Instant};
    objc2::rc::autoreleasepool(|_| {
        for id in [
            "dev.kanade.inputmethod",
            "dev.kanade.inputmethod.Kanade",
            "dev.kanade.settings",
        ] {
            let allowed: Vec<_> = [source, target, previous]
                .into_iter()
                .map(|p| {
                    if id == "dev.kanade.settings" {
                        p.join("Contents/Helpers/Settings.app")
                    } else {
                        p.to_path_buf()
                    }
                })
                .filter_map(|p| p.canonicalize().ok())
                .collect();
            let applications = NSRunningApplication::runningApplicationsWithBundleIdentifier(
                &NSString::from_str(id),
            );
            for index in 0..applications.count() {
                let application = applications.objectAtIndex(index);
                let path = application
                    .bundleURL()
                    .and_then(|url| url.path())
                    .map(|path| PathBuf::from(path.to_string()))
                    .and_then(|path| path.canonicalize().ok());
                ensure!(
                    path.as_ref().is_some_and(|path| allowed.contains(path)),
                    "another Kanade installation is running; close it before updating"
                );
                if application.isTerminated() {
                    continue;
                }
                ensure!(
                    application.terminate(),
                    "the previous IME did not accept a quit request"
                );
                let started = Instant::now();
                while !application.isTerminated() {
                    ensure!(
                        started.elapsed() < Duration::from_secs(5),
                        "the previous IME is still running; close it before updating"
                    );
                    NSRunLoop::currentRunLoop()
                        .runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.1));
                }
            }
        }
        Ok(())
    })
}

#[cfg(target_os = "macos")]
fn inspect_input_source(path: &Path, register: bool, enable: bool) -> Result<serde_json::Value> {
    let metadata: serde_json::Value = serde_json::from_slice(
        &fs::read(path.join("Contents/Resources/kanade-build.json"))
            .context("update the installed bundle before checking its registration")?,
    )?;
    ensure!(
        metadata["registration_cli"] == 1,
        "unsupported input-source registration helper"
    );
    let mut command = Command::new(path.join("Contents/MacOS/kanade-imk"));
    command.arg(if register {
        "--register-input-source"
    } else {
        "--input-source-status"
    });
    if enable {
        command.arg("--enable");
    }
    let result = command.output()?;
    ensure!(
        result.status.success(),
        "input-source helper failed: {}",
        String::from_utf8_lossy(&result.stderr).trim()
    );
    Ok(serde_json::from_slice(&result.stdout)?)
}
#[cfg(target_os = "macos")]
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
#[cfg(target_os = "macos")]
fn restart_agent(domain: &str, service: &str, path: &Path) -> Result<()> {
    use std::time::{Duration, Instant};
    let _ = Command::new("launchctl")
        .args(["bootout", service])
        .output()?;
    let started = Instant::now();
    loop {
        let result = Command::new("launchctl")
            .args(["bootstrap", domain])
            .arg(path)
            .output()?;
        if result.status.success() {
            return Ok(());
        }
        ensure!(
            started.elapsed() < Duration::from_secs(5),
            "engine LaunchAgent could not restart: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        );
        // launchd may still be removing the old service after bootout returns.
        std::thread::sleep(Duration::from_millis(100));
    }
}
#[cfg(target_os = "macos")]
fn verify_engine(path: &Path) -> Result<()> {
    let started = std::time::Instant::now();
    loop {
        let status =
            Command::new(path.join("Contents/Helpers/Settings.app/Contents/MacOS/kanade-ui"))
                .arg("settings")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()?;
        if status.success() {
            return Ok(());
        }
        ensure!(
            started.elapsed() < std::time::Duration::from_secs(15),
            "installed settings client could not connect to the engine"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}
pub fn macos(root: &Path) -> Result<()> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = root;
        bail!("macOS installation requires a Mac");
    }
    #[cfg(target_os = "macos")]
    {
        if input_source::helper()? {
            return Ok(());
        }
        let enable = std::env::args().any(|arg| arg == "--enable");
        let preview_package = std::env::args().any(|arg| arg == "--preview-pkg");
        let home = PathBuf::from(std::env::var_os("HOME").context("home directory missing")?);
        let methods = std::env::var_os("KANADE_INSTALL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("Library/Input Methods"));
        ensure!(
            methods.is_absolute(),
            "installation directory must be absolute"
        );
        if std::env::args().any(|arg| arg == "--status") {
            let status = discover_input_source()?;
            println!("{}", serde_json::to_string(&status)?);
            return Ok(());
        }
        let data = std::env::var_os("KANADE_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("Library/Application Support/kanade-ime"));
        ensure!(
            data.is_absolute(),
            "application data directory must be absolute"
        );
        fs::create_dir_all(&methods)?;
        let install_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(methods.join(".kanade-install.lock"))?;
        install_lock
            .try_lock()
            .context("another Kanade installer is running")?;
        if std::env::args().any(|arg| arg == "--uninstall") {
            let target = methods.join("Kanade.app");
            if !target.exists() {
                println!("Kanade is not installed in this directory");
                return Ok(());
            }
            let bundle_id = output(
                Command::new("/usr/libexec/PlistBuddy")
                    .args(["-c", "Print :CFBundleIdentifier"])
                    .arg(target.join("Contents/Info.plist")),
            )?;
            ensure!(
                matches!(
                    bundle_id.as_str(),
                    "dev.kanade.inputmethod" | "dev.kanade.inputmethod.Kanade"
                ),
                "the bundle belongs to a different application"
            );
            let agent = home.join("Library/LaunchAgents/dev.kanade.engine.plist");
            if agent.exists() {
                let program = output(
                    Command::new("/usr/libexec/PlistBuddy")
                        .args(["-c", "Print :ProgramArguments:0"])
                        .arg(&agent),
                )?;
                ensure!(
                    Path::new(&program) == target.join("Contents/MacOS/kanade-engine"),
                    "the LaunchAgent belongs to another installation"
                );
            }
            if std::env::args().any(|arg| arg == "--dry-run") {
                println!(
                    "uninstall preflight passed; no application or input-source state was changed"
                );
                return Ok(());
            }
            let uid = output(Command::new("id").arg("-u"))?;
            let domain = format!("gui/{uid}");
            let service = format!("{domain}/dev.kanade.engine");
            let result = input_source::during_update(|| {
                stop_previous_frontends(&target, &target, &methods.join(".Kanade.previous"))?;
                if Command::new("launchctl")
                    .args(["print", &service])
                    .output()?
                    .status
                    .success()
                {
                    run(Command::new("launchctl").args(["bootout", &service]))?;
                }
                unregister_build_copy(&target)?;
                fs::remove_dir_all(&target)?;
                if agent.exists() {
                    fs::remove_file(&agent)?;
                }
                println!("removed Kanade; personal data was retained");
                Ok(())
            });
            if result.is_err() && target.is_dir() && agent.is_file() {
                restart_agent(&domain, &service, &agent)
                    .context("failed to restore the engine after uninstall failure")?;
            }
            return result;
        }
        let arguments: Vec<_> = std::env::args().collect();
        let source = if let Some(index) = arguments.iter().position(|arg| arg == "--bundle") {
            let path = PathBuf::from(
                arguments
                    .get(index + 1)
                    .context("--bundle requires a path")?,
            );
            ensure!(path.is_absolute(), "bundle path must be absolute");
            path.canonicalize()?
        } else {
            root.join("artifacts/macos.noindex/Kanade.app")
        };
        let bundle_id = output(
            Command::new("/usr/libexec/PlistBuddy")
                .args(["-c", "Print :CFBundleIdentifier"])
                .arg(source.join("Contents/Info.plist")),
        )?;
        ensure!(
            matches!(
                bundle_id.as_str(),
                "dev.kanade.inputmethod" | "dev.kanade.inputmethod.Kanade"
            ),
            "the source bundle belongs to a different application"
        );
        run(Command::new("codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&source))?;
        let signing = Command::new("codesign")
            .args(["-d", "--verbose=2"])
            .arg(&source)
            .output()?;
        ensure!(
            signing.status.success()
                && String::from_utf8_lossy(&signing.stderr)
                    .lines()
                    .any(|line| line.starts_with("Authority=")),
            "installation requires a certificate-signed bundle"
        );
        if std::env::args().any(|arg| arg == "--dry-run") {
            println!("install preflight passed; no application or input-source state was changed");
            return Ok(());
        }
        unregister_build_copy(&source)?;
        let staging = methods.join("Kanade.next.app");
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        run(Command::new("ditto").arg(&source).arg(&staging))?;
        let target = methods.join("Kanade.app");
        let previous = methods.join(".Kanade.previous");
        // A backup ending in .app would be discovered as a duplicate input method.
        let legacy = methods.join("Kanade.previous.app");
        if legacy.exists() {
            let id = output(
                Command::new("/usr/libexec/PlistBuddy")
                    .args(["-c", "Print :CFBundleIdentifier"])
                    .arg(legacy.join("Contents/Info.plist")),
            )?;
            ensure!(
                id.starts_with("dev.kanade.inputmethod"),
                "the legacy backup belongs to a different application"
            );
            fs::remove_dir_all(&legacy)?;
        }
        if previous.exists() {
            fs::remove_dir_all(&previous)?;
        }
        input_source::during_update(|| {
            stop_previous_frontends(&source, &target, &previous)?;
            if target.exists() {
                fs::rename(&target, &previous)?;
            }
            if let Err(error) = fs::rename(&staging, &target) {
                if previous.exists() {
                    fs::rename(&previous, &target)
                        .context("could not restore the previous bundle")?;
                }
                return Err(error.into());
            }
            if let Err(error) = inspect_input_source(&target, false, false) {
                if previous.exists() {
                    fs::remove_dir_all(&target)?;
                    fs::rename(&previous, &target)?;
                }
                return Err(error);
            }
            let agents = home.join("Library/LaunchAgents");
            fs::create_dir_all(&agents)?;
            let path = agents.join("dev.kanade.engine.plist");
            let engine = xml(&target
                .join("Contents/MacOS/kanade-engine")
                .to_string_lossy());
            let data = xml(&data.to_string_lossy());
            let temporary = xml(&output(
                Command::new("getconf").arg("DARWIN_USER_TEMP_DIR"),
            )?);
            let plist = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>dev.kanade.engine</string>
<key>ProgramArguments</key><array><string>{engine}</string></array>
<key>EnvironmentVariables</key><dict>
<key>KANADE_DATA_DIR</key><string>{data}</string>
<key>TMPDIR</key><string>{temporary}</string>
</dict>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
<key>ThrottleInterval</key><integer>10</integer>
</dict></plist>"#
            );
            fs::write(&path, plist)?;
            let uid = output(Command::new("id").arg("-u"))?;
            let domain = format!("gui/{uid}");
            let service = format!("{domain}/dev.kanade.engine");
            restart_agent(&domain, &service, &path)?;
            verify_engine(&target)?;
            if let Err(error) = inspect_input_source(&target, true, enable) {
                if preview_package {
                    println!(
                        "Kanade was installed; log in again and add Kanade in System Settings"
                    );
                    return Ok(());
                }
                return Err(error);
            }
            let started = std::time::Instant::now();
            let global = loop {
                let status: serde_json::Value = serde_json::from_str(&output(
                    Command::new(std::env::current_exe()?).args(["install-macos", "--status"]),
                )?)?;
                if status["installed"].as_u64().unwrap_or(0) > 0
                    || started.elapsed() >= std::time::Duration::from_secs(5)
                {
                    break status;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            };
            println!("input-source status: {}", serde_json::to_string(&global)?);
            if global["installed"].as_u64().unwrap_or(0) == 0 {
                println!("input-source discovery is pending; log in again before adding Kanade");
            } else if enable && global["enabled"].as_u64().unwrap_or(0) == 0 {
                println!("input-source enabling is pending; add Kanade in System Settings");
            } else if global["enabled"].as_u64().unwrap_or(0) > 0
                && global["enabled_modes"].as_array().is_none_or(Vec::is_empty)
            {
                println!(
                    "Kanade's input mode is missing from the enabled list; remove and re-add Kanade in System Settings"
                );
            }
            println!("installed Kanade; input-source discovery status is shown above");
            Ok(())
        })
    }
}
