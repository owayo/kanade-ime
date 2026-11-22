//! Finds the programs installed with the text service and starts the settings
//! application and the engine's supervisor. Nothing here reads input or
//! documents.
use super::*;
use std::{
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Security::{
            GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_DUPLICATE,
            TOKEN_ELEVATION, TOKEN_ELEVATION_TYPE, TOKEN_LINKED_TOKEN, TOKEN_MANDATORY_LABEL,
            TOKEN_QUERY, TokenElevation, TokenElevationType, TokenElevationTypeLimited,
            TokenHasRestrictions, TokenIntegrityLevel, TokenIsAppContainer, TokenLinkedToken,
        },
        System::{
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            JobObjects::IsProcessInJob,
            LibraryLoader::{GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GetModuleHandleExW},
            Threading::*,
        },
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
// first and less than the second; an elevated one has the second.
const MEDIUM: u32 = 0x2000;
const HIGH: u32 = 0x3000;
const SYSTEM: u32 = 0x4000;

/// A sandboxed host would start the settings application inside its own
/// sandbox, and the hosts of the secure desktop run as the system. Neither
/// may start it.
pub(super) fn may_launch(secure: bool, app_container: bool, integrity: u32) -> bool {
    !secure && !app_container && (MEDIUM..SYSTEM).contains(&integrity)
}

/// Use the user's logon environment so a host-specific data directory or
/// engine override cannot affect the engine shared by other applications.
fn user_environment() -> Option<Vec<u16>> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY | TOKEN_DUPLICATE,
            &mut token,
        )
        .ok()?;
        let token = Handle(token);
        let mut block = std::ptr::null_mut();
        // Without the variables of this process.
        CreateEnvironmentBlock(&mut block, Some(token.0), false).ok()?;
        let copy = filtered_environment(block.cast());
        let _ = DestroyEnvironmentBlock(block);
        Some(copy)
    }
}

/// The caller supplies a double-NUL-terminated Windows environment block.
unsafe fn filtered_environment(mut block: *const u16) -> Vec<u16> {
    let mut copy = Vec::new();
    unsafe {
        while *block != 0 {
            let name_value = PCWSTR(block);
            let entry = name_value.as_wide();
            let override_name = entry.get(..7).is_some_and(|prefix| {
                prefix.iter().zip(b"KANADE_").all(|(&unit, ascii)| {
                    unit == u16::from(*ascii) || unit == u16::from(ascii.to_ascii_lowercase())
                })
            });
            if !override_name {
                copy.extend_from_slice(entry);
                copy.push(0);
            }
            block = block.add(entry.len() + 1);
        }
    }
    if copy.is_empty() {
        copy.push(0);
    }
    copy.push(0);
    copy
}

/// Opens the pipe of the installed engine. When no engine serves the pipe its
/// supervisor may be gone too. Only delivery of an owned key can start it.
pub(super) fn engine(allow_start: bool) -> Option<Pipe> {
    Pipe::connect_or_start(&installed_program("kanade-engine.exe")?, || {
        if allow_start {
            start_engine()
        }
    })
    .ok()
}

/// How long a process waits before it starts the supervisor again.
const START_AGAIN: Duration = Duration::from_secs(10);
enum Launch {
    Ready,
    Pending,
    Finished(Instant),
}

impl Launch {
    fn reserve(&mut self, now: Instant) -> bool {
        match self {
            Self::Pending => false,
            Self::Finished(at) if now.saturating_duration_since(*at) < START_AGAIN => false,
            _ => {
                *self = Self::Pending;
                true
            }
        }
    }
}

static LAUNCH: Mutex<Launch> = Mutex::new(Launch::Ready);

fn start_engine() {
    if !LAUNCH
        .try_lock()
        .is_ok_and(|mut launch| launch.reserve(Instant::now()))
    {
        return;
    }
    // The host may unload the TIP immediately after returning from the key
    // sink. Keep the DLL mapped through the callback's return instruction.
    unsafe {
        let mut module = HMODULE::default();
        if GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            PCWSTR(launch_supervisor as *const () as *const u16),
            &mut module,
        )
        .is_ok()
        {
            if TrySubmitThreadpoolCallback(Some(launch_supervisor), Some(module.0), None).is_ok() {
                return;
            }
            let _ = FreeLibrary(module);
        }
    }
    launch_finished();
}

fn launch_finished() {
    if let Ok(mut state) = LAUNCH.lock() {
        *state = Launch::Finished(Instant::now());
    }
}

unsafe extern "system" fn launch_supervisor(callback: PTP_CALLBACK_INSTANCE, module: *mut c_void) {
    unsafe {
        FreeLibraryWhenCallbackReturns(callback, HMODULE(module));
    }
    if host().is_ok_and(|host| host.may_start_engine())
        && let Some(engine) = installed("kanade-engine.exe")
        && let Some(environment) = user_environment()
        && let Ok(process) = start(&engine, " --supervise", Some(&environment), false)
    {
        unsafe {
            let _ = CloseHandle(process.hThread);
            let _ = CloseHandle(process.hProcess);
        }
    }
    launch_finished();
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct HostToken {
    container: bool,
    integrity: u32,
    elevated: bool,
    restricted: bool,
}

impl HostToken {
    fn may_start_engine(&self) -> bool {
        !self.restricted
            && !self.container
            && !self.elevated
            && (MEDIUM..HIGH).contains(&self.integrity)
    }
}

// A UAC split token is filtered too. Compare it with the standard limited
// token of its logon, reached via the elevated token's link back. Additional
// restricting SIDs, disabled groups or removed privileges fail that comparison.
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCompareTokens(first: HANDLE, second: HANDLE, equal: *mut u8) -> i32;
}

fn linked(token: &Handle) -> Result<Handle> {
    let mut linked = TOKEN_LINKED_TOKEN::default();
    unsafe {
        GetTokenInformation(
            token.0,
            TokenLinkedToken,
            Some((&raw mut linked).cast()),
            std::mem::size_of_val(&linked) as u32,
            &mut 0,
        )?;
    }
    Ok(Handle(linked.LinkedToken))
}

fn standard_limited_token(token: &Handle) -> Result<bool> {
    let mut kind = TOKEN_ELEVATION_TYPE::default();
    unsafe {
        GetTokenInformation(
            token.0,
            TokenElevationType,
            Some((&raw mut kind).cast()),
            std::mem::size_of_val(&kind) as u32,
            &mut 0,
        )?;
        if kind != TokenElevationTypeLimited {
            return Ok(false);
        }
        let standard = linked(&linked(token)?)?;
        let mut equal = 0;
        Ok(NtCompareTokens(standard.0, token.0, &mut equal) == 0 && equal != 0)
    }
}

fn process_token() -> Result<Handle> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
    }
    Ok(Handle(token))
}

fn host() -> Result<HostToken> {
    read_host_token(&process_token()?)
}

fn desktop(token: &Handle) -> Result<(bool, u32)> {
    unsafe {
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

fn read_host_token(token: &Handle) -> Result<HostToken> {
    let (container, integrity) = desktop(token)?;
    let mut len = 0;
    unsafe {
        let mut elevation = TOKEN_ELEVATION::default();
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some((&raw mut elevation).cast()),
            std::mem::size_of_val(&elevation) as u32,
            &mut len,
        )?;
        let mut restricted = 0u32;
        GetTokenInformation(
            token.0,
            TokenHasRestrictions,
            Some((&raw mut restricted).cast()),
            std::mem::size_of_val(&restricted) as u32,
            &mut len,
        )?;
        Ok(HostToken {
            container,
            integrity,
            elevated: elevation.TokenIsElevated != 0,
            restricted: restricted != 0 && !standard_limited_token(token)?,
        })
    }
}

/// Whether this host may start the settings application. `secure` is the
/// activation flag of the secure desktop.
pub(super) fn host_may_launch(secure: bool) -> bool {
    process_token()
        .and_then(|token| desktop(&token))
        .is_ok_and(|(container, integrity)| may_launch(secure, container, integrity))
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
    // The user asked this host for the settings: they open inside its job
    // rather than not at all.
    let process = start(&program, "", None, true)?;
    unsafe {
        // The click came from this process, so it may hand the foreground on.
        let _ = AllowSetForegroundWindow(process.dwProcessId);
        let _ = CloseHandle(process.hThread);
        let _ = CloseHandle(process.hProcess);
    }
    Ok(())
}

/// The creation flags to try, in order. A host can run in a job, which ends
/// its processes with it or limits them, so a program is started outside
/// the job. A job that forbids leaving refuses that; only a program that
/// may run `in_job` is then started inside it.
fn attempts(
    base: PROCESS_CREATION_FLAGS,
    in_job: bool,
) -> impl Iterator<Item = PROCESS_CREATION_FLAGS> {
    let mut leaving = base | CREATE_BREAKAWAY_FROM_JOB;
    if !in_job {
        // The shared supervisor and its engine must not inherit a low host priority.
        leaving |= CREATE_SUSPENDED | NORMAL_PRIORITY_CLASS;
    }
    std::iter::once(leaving).chain(in_job.then_some(base))
}

/// Starts a program installed with this text service, without a console
/// window and without waiting for it. The command line is the quoted program
/// path followed by `arguments`, which are fixed by the caller. Without an
/// `environment` block the program inherits this process's environment.
/// `in_job` is as for `attempts`.
fn start(
    program: &Path,
    arguments: &str,
    environment: Option<&[u16]>,
    in_job: bool,
) -> Result<PROCESS_INFORMATION> {
    let directory = program.parent().ok_or_else(failure)?;
    let application = wide(program.as_os_str());
    let mut command = vec![u16::from(b'"')];
    command.extend(application.iter().take_while(|unit| **unit != 0));
    command.push(u16::from(b'"'));
    command.extend(arguments.encode_utf16());
    command.push(0);
    let directory = wide(directory.as_os_str());
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // The settings application is a console program: start it without a
    // console window.
    let mut base = CREATE_NO_WINDOW | CREATE_DEFAULT_ERROR_MODE;
    if environment.is_some() {
        base |= CREATE_UNICODE_ENVIRONMENT;
    }
    let mut started = Err(failure());
    for flags in attempts(base, in_job) {
        started = unsafe {
            CreateProcessW(
                PCWSTR(application.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                false,
                flags,
                environment.map(|block| block.as_ptr().cast()),
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
    if !in_job {
        // Breakaway can leave only the innermost job. Check the child before
        // its first instruction so it cannot start an engine in an outer job.
        unsafe {
            let mut member = BOOL::default();
            let detached = IsProcessInJob(process.hProcess, None, &mut member);
            if detached.is_err() || member.as_bool() || ResumeThread(process.hThread) == u32::MAX {
                let _ = TerminateProcess(process.hProcess, 1);
                let _ = CloseHandle(process.hThread);
                let _ = CloseHandle(process.hProcess);
                return Err(failure());
            }
        }
    }
    Ok(process)
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
    fn only_an_ordinary_unelevated_host_starts_the_engine() {
        const LOW: u32 = 0x1000;
        let allowed = |container, integrity, elevated| {
            HostToken {
                container,
                integrity,
                elevated,
                restricted: false,
            }
            .may_start_engine()
        };
        assert!(allowed(false, MEDIUM, false));
        assert!(allowed(false, MEDIUM + 0x100, false));
        assert!(!allowed(false, MEDIUM, true));
        assert!(!allowed(false, HIGH, false));
        assert!(!allowed(true, MEDIUM, false));
        assert!(!allowed(false, LOW, false));
        assert!(!allowed(false, SYSTEM, false));
    }

    #[test]
    fn the_user_environment_excludes_kanade_overrides_in_any_case() {
        let raw: Vec<u16> = "kanaDE_Instance=test\0KANADE_WORKER=x\0X_KANADE_A=b\0Path=a\0\0"
            .encode_utf16()
            .collect();
        let expected: Vec<u16> = "X_KANADE_A=b\0Path=a\0\0".encode_utf16().collect();
        let block = unsafe { filtered_environment(raw.as_ptr()) };
        assert!(block == expected);
        let only_override: Vec<u16> = "kanade_a=x\0\0".encode_utf16().collect();
        assert!(unsafe { filtered_environment(only_override.as_ptr()) } == [0, 0]);
    }

    #[test]
    fn launch_attempts_are_serial_and_ten_seconds_apart() {
        let now = Instant::now();
        let mut state = Launch::Ready;
        assert!(state.reserve(now));
        assert!(!state.reserve(now + START_AGAIN));
        state = Launch::Finished(now);
        assert!(!state.reserve(now + START_AGAIN - Duration::from_millis(1)));
        assert!(state.reserve(now + START_AGAIN));
    }

    #[test]
    fn the_supervisor_gets_the_environment_of_the_user_not_of_the_host() -> Result<()> {
        use windows::Win32::UI::Shell::{
            FOLDERID_LocalAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
        };
        let block = user_environment().ok_or_else(failure)?;
        assert!(block.ends_with(&[0, 0]));
        let mut variables = block
            .split(|u| *u == 0)
            .take_while(|v| !v.is_empty())
            .map(String::from_utf16_lossy);
        assert!(
            !variables
                .clone()
                .any(|var| var.to_ascii_uppercase().starts_with("KANADE_"))
        );
        // The data directory of the engine is the one Windows gives this
        // user, whatever a host calls it.
        let folder =
            unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None)? };
        let expected = unsafe { folder.to_string() };
        unsafe { CoTaskMemFree(Some(folder.0.cast())) };
        let data = expected.map(|path| format!("LOCALAPPDATA={path}"))?;
        assert!(variables.any(|var| var.eq_ignore_ascii_case(&data)));
        Ok(())
    }

    #[test]
    fn only_a_program_that_may_stay_is_started_inside_the_job_of_the_host() {
        let base = CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT;
        let leaving = base | CREATE_BREAKAWAY_FROM_JOB;
        // The supervisor: outside the job or not at all.
        assert_eq!(
            attempts(base, false).collect::<Vec<_>>(),
            [leaving | CREATE_SUSPENDED | NORMAL_PRIORITY_CLASS]
        );
        // The settings application: outside first, inside otherwise.
        assert_eq!(attempts(base, true).collect::<Vec<_>>(), [leaving, base]);
    }

    #[test]
    fn low_priority_host_starts_a_normal_priority_supervisor() -> Result<()> {
        use std::os::windows::{io::AsRawHandle, process::CommandExt};
        const LOW_HOST: &str = "KANADE_TEST_LOW_PRIORITY_HOST";
        let image = std::env::current_exe().map_err(|_| failure())?;
        if std::env::var_os(LOW_HOST).is_some() {
            let priority = unsafe { GetPriorityClass(GetCurrentProcess()) };
            assert!([IDLE_PRIORITY_CLASS.0, BELOW_NORMAL_PRIORITY_CLASS.0].contains(&priority));
            // The test runner can forbid breakaway. Keep the suspended probe in
            // its job and inspect the priority Windows actually gave the child.
            let flags = attempts(CREATE_NO_WINDOW, false)
                .next()
                .ok_or_else(failure)?;
            let mut child = std::process::Command::new(&image)
                .arg("--list")
                .creation_flags((flags & !CREATE_BREAKAWAY_FROM_JOB).0)
                .spawn()
                .map_err(|_| failure())?;
            let actual = unsafe { GetPriorityClass(HANDLE(child.as_raw_handle())) };
            child
                .kill()
                .and_then(|_| child.wait())
                .map_err(|_| failure())?;
            assert_eq!(actual, NORMAL_PRIORITY_CLASS.0);
        } else {
            let mut passed = true;
            for priority in [IDLE_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS] {
                let status = std::process::Command::new(&image)
                    .arg("--exact")
                    .arg("native::programs::tests::low_priority_host_starts_a_normal_priority_supervisor")
                    .env(LOW_HOST, "1")
                    .creation_flags((CREATE_NO_WINDOW | priority).0)
                    .status()
                    .map_err(|_| failure())?;
                passed &= status.success();
            }
            assert!(passed);
        }
        Ok(())
    }

    const JOB_TEST: &str = "KANADE_TEST_JOB";

    // The helper joins the jobs in its own disposable process.
    #[test]
    #[ignore = "run in a child process by supervisor_requires_breakaway_from_every_job"]
    fn inside_restrictive_job() -> Result<()> {
        use windows::Win32::System::JobObjects::*;
        let Ok(kind) = std::env::var(JOB_TEST) else {
            return Ok(());
        };
        unsafe {
            let outer = Handle(CreateJobObjectW(None, PCWSTR::null())?);
            AssignProcessToJobObject(outer.0, GetCurrentProcess())?;
            if kind == "nested" {
                let inner = Handle(CreateJobObjectW(None, PCWSTR::null())?);
                let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
                SetInformationJobObject(
                    inner.0,
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    std::mem::size_of_val(&limits) as u32,
                )?;
                AssignProcessToJobObject(inner.0, GetCurrentProcess())?;
            }
        }
        let root = std::env::var_os("SystemRoot").ok_or_else(failure)?;
        let program = PathBuf::from(root).join("System32").join("cmd.exe");
        assert!(start(&program, " /d /c exit", None, false).is_err());
        // The settings application can fall back to its host job.
        let process = start(&program, " /d /c exit", None, true)?;
        drop((Handle(process.hThread), Handle(process.hProcess)));
        std::process::exit(73);
    }

    #[test]
    fn supervisor_requires_breakaway_from_every_job() -> Result<()> {
        use std::os::windows::process::CommandExt;
        for kind in ["single", "nested"] {
            let status =
                std::process::Command::new(std::env::current_exe().map_err(|_| failure())?)
                    .args([
                        "--exact",
                        "native::programs::tests::inside_restrictive_job",
                        "--ignored",
                    ])
                    .env(JOB_TEST, kind)
                    .creation_flags(CREATE_NO_WINDOW.0)
                    .status()
                    .map_err(|_| failure())?;
            assert_eq!(status.code(), Some(73));
        }
        Ok(())
    }

    #[test]
    fn the_host_token_can_be_read() -> Result<()> {
        let host = host()?;
        // A test process is neither sandboxed nor below an ordinary user.
        assert!(!host.container);
        assert!(host.integrity >= MEDIUM);
        assert!(!host_may_launch(true));
        Ok(())
    }

    #[test]
    fn a_standard_uac_limited_token_can_start_the_supervisor() -> Result<()> {
        let current = process_token()?;
        // A standard-user logon has no split token. When there is a linked
        // pair, read the standard limited member even in an elevated runner.
        if let Ok(other) = linked(&current) {
            let limited = if read_host_token(&other)?.elevated {
                linked(&other)?
            } else {
                other
            };
            assert!(read_host_token(&limited)?.may_start_engine());
        }
        Ok(())
    }

    #[test]
    fn filtered_tokens_cannot_start_the_supervisor() -> Result<()> {
        use windows::Win32::Security::*;
        unsafe {
            let mut original = HANDLE::default();
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE | TOKEN_QUERY,
                &mut original,
            )?;
            let original = Handle(original);
            let mut storage = [0u32; 17];
            let sid = PSID(storage.as_mut_ptr().cast());
            let mut bytes = std::mem::size_of_val(&storage) as u32;
            CreateWellKnownSid(WinWorldSid, None, Some(sid), &mut bytes)?;
            let sids = [SID_AND_ATTRIBUTES {
                Sid: sid,
                Attributes: 0,
            }];
            for (flags, deny, restrict) in [
                (CREATE_RESTRICTED_TOKEN_FLAGS(0), None, Some(&sids[..])),
                (CREATE_RESTRICTED_TOKEN_FLAGS(0), Some(&sids[..]), None),
                (DISABLE_MAX_PRIVILEGE, None, None),
            ] {
                let mut restricted = HANDLE::default();
                CreateRestrictedToken(original.0, flags, deny, None, restrict, &mut restricted)?;
                let mut host = read_host_token(&Handle(restricted))?;
                host.integrity = MEDIUM;
                host.elevated = false;
                assert!(!host.may_start_engine());
            }
            Ok(())
        }
    }
}
