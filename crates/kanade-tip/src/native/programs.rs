//! Finds the programs installed with the text service and starts the settings
//! application. Nothing here reads input or documents.
use super::*;
use std::path::Path;
use windows::{
    Win32::{
        Security::{
            GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
            TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel, TokenIsAppContainer,
        },
        System::Threading::*,
        UI::WindowsAndMessaging::AllowSetForegroundWindow,
    },
    core::{PCWSTR, PWSTR},
};

/// The directory that holds the engine and the settings application. The
/// 32-bit text service is installed in `x86` under it.
pub(super) fn install_root(module: &Path) -> Option<&Path> {
    let folder = module.parent()?;
    if folder
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("x86"))
    {
        folder.parent()
    } else {
        Some(folder)
    }
}

/// The path of a program installed with this text service.
pub(super) fn installed(name: &str) -> Option<PathBuf> {
    Some(install_root(&module_path().ok()?)?.join(name))
}

/// A program installed with this text service, named for comparison with
/// the image of another process. A sandboxed host may not see the drive
/// letter of the volume, so the program is named by the native path next to
/// this module, which needs none. The Win32 path stands in only when this
/// module's native path cannot be read.
pub(super) fn installed_program(name: &str) -> Option<kanade_ipc::windows::Program> {
    use kanade_ipc::windows::{Program, native_path_of};
    match native_path_of(MODULE.load(Ordering::Acquire)) {
        Ok(native) => Some(Program::Native(install_root(&native)?.join(name))),
        Err(_) => installed(name).map(Program::Win32),
    }
}

// Mandatory integrity levels: an ordinary desktop process has at least the
// first and less than the second.
const MEDIUM: u32 = 0x2000;
const SYSTEM: u32 = 0x4000;

/// A sandboxed host would start the settings application inside its own
/// sandbox, and the hosts of the secure desktop run as the system. Neither
/// may start it.
pub(super) fn may_launch(secure: bool, app_container: bool, integrity: u32) -> bool {
    !secure && !app_container && (MEDIUM..SYSTEM).contains(&integrity)
}

struct Token(HANDLE);
impl Drop for Token {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Whether the host process is an AppContainer, and its integrity level.
fn host() -> Result<(bool, u32)> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let token = Token(token);
        let mut container = 0u32;
        let mut len = 0;
        GetTokenInformation(
            token.0,
            TokenIsAppContainer,
            Some((&raw mut container).cast()),
            std::mem::size_of::<u32>() as u32,
            &mut len,
        )?;
        // The label is followed by its SID; 8-byte units keep both aligned.
        let mut label = [0u64; 16];
        GetTokenInformation(
            token.0,
            TokenIntegrityLevel,
            Some(label.as_mut_ptr().cast()),
            std::mem::size_of_val(&label) as u32,
            &mut len,
        )?;
        let sid = (*label.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid;
        let count = GetSidSubAuthorityCount(sid);
        if count.is_null() || *count == 0 {
            return Err(failure());
        }
        let level = GetSidSubAuthority(sid, u32::from(*count) - 1);
        if level.is_null() {
            return Err(failure());
        }
        Ok((container != 0, *level))
    }
}

/// Whether this host may start the settings application. `secure` is the
/// activation flag of the secure desktop.
pub(super) fn host_may_launch(secure: bool) -> bool {
    host().is_ok_and(|(container, integrity)| may_launch(secure, container, integrity))
}

const SETTINGS: &str = "kanade-ui.exe";

/// Whether the settings application is installed beside the engine.
pub(super) fn settings_installed() -> bool {
    installed(SETTINGS).is_some_and(|program| program.is_file())
}

fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    text.encode_wide().chain(Some(0)).collect()
}

/// Starts the settings application installed beside the engine and returns
/// without waiting for it. The application runs as the host does.
pub(super) fn open_settings() -> Result<()> {
    let program = installed(SETTINGS).ok_or_else(failure)?;
    let directory = program.parent().ok_or_else(failure)?;
    let application = wide(program.as_os_str());
    // The command line is the quoted program path alone.
    let mut command = vec![u16::from(b'"')];
    command.extend(application.iter().take_while(|unit| **unit != 0));
    command.extend([u16::from(b'"'), 0]);
    let directory = wide(directory.as_os_str());
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // The settings application is a console program: start it without a
    // console window. It should outlive a host that runs in a job; a job that
    // forbids leaving it refuses the first attempt.
    let base = CREATE_NO_WINDOW | CREATE_DEFAULT_ERROR_MODE;
    let mut started = Err(failure());
    for flags in [base | CREATE_BREAKAWAY_FROM_JOB, base] {
        started = unsafe {
            CreateProcessW(
                PCWSTR(application.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                false,
                flags,
                None,
                PCWSTR(directory.as_ptr()),
                &startup,
                &mut process,
            )
        };
        if started.is_ok() {
            break;
        }
    }
    started?;
    unsafe {
        // The click came from this process, so it may hand the foreground on.
        let _ = AllowSetForegroundWindow(process.dwProcessId);
        let _ = CloseHandle(process.hThread);
        let _ = CloseHandle(process.hProcess);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_are_found_beside_either_text_service() {
        let root = Path::new(r"C:\Program Files\Kanade");
        for module in [
            r"C:\Program Files\Kanade\kanade_tip.dll",
            r"C:\Program Files\Kanade\x86\kanade_tip.dll",
            r"C:\Program Files\Kanade\X86\kanade_tip.dll",
        ] {
            assert_eq!(install_root(Path::new(module)), Some(root));
        }
        // Only the directory that holds the 32-bit text service is skipped.
        assert_eq!(
            install_root(Path::new(r"C:\x86\Kanade\kanade_tip.dll")),
            Some(Path::new(r"C:\x86\Kanade"))
        );
        assert_eq!(install_root(Path::new("")), None);
        // A native path has no drive letter and is found the same way.
        let native = Path::new(r"\Device\HarddiskVolume3\Program Files\Kanade");
        for module in [
            r"\Device\HarddiskVolume3\Program Files\Kanade\kanade_tip.dll",
            r"\Device\HarddiskVolume3\Program Files\Kanade\x86\kanade_tip.dll",
        ] {
            assert_eq!(install_root(Path::new(module)), Some(native));
        }
    }

    #[test]
    fn the_engine_is_named_by_the_native_path_beside_this_module() -> Result<()> {
        use kanade_ipc::windows::{Program, native_executable};
        // A test binary is not loaded as a module: its executable stands in.
        // A null module also names the executable, so other tests that read
        // the module see the same file.
        let handle =
            unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(PCWSTR::null()) }?;
        MODULE.store(handle.0 as usize, Ordering::Release);
        let native = native_executable().map_err(|_| failure())?;
        assert_eq!(
            installed_program("kanade-engine.exe"),
            Some(Program::Native(native.with_file_name("kanade-engine.exe")))
        );
        Ok(())
    }

    #[test]
    fn only_an_ordinary_desktop_host_starts_the_settings_application() {
        const LOW: u32 = 0x1000;
        const HIGH: u32 = 0x3000;
        assert!(may_launch(false, false, MEDIUM));
        assert!(may_launch(false, false, MEDIUM + 0x100));
        assert!(may_launch(false, false, HIGH));
        // The secure desktop, an AppContainer, a low-integrity sandbox and a
        // system process.
        assert!(!may_launch(true, false, MEDIUM));
        assert!(!may_launch(false, true, MEDIUM));
        assert!(!may_launch(false, false, LOW));
        assert!(!may_launch(false, false, 0));
        assert!(!may_launch(false, false, SYSTEM));
        assert!(!may_launch(false, false, 0x5000));
    }

    #[test]
    fn the_host_token_can_be_read() -> Result<()> {
        let (container, integrity) = host()?;
        // A test process is neither sandboxed nor below an ordinary user.
        assert!(!container);
        assert!(integrity >= MEDIUM);
        assert!(!host_may_launch(true));
        Ok(())
    }
}
