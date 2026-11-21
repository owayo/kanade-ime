// SPDX-License-Identifier: MIT
//! Runs a program in an AppContainer, the sandbox of packaged applications
//! and of the system's own search and settings windows.
//!
//! The documented way to start such a process needs a container profile
//! registered for the user. Tests must not register anything, so this
//! starts the program suspended as an ordinary process and replaces its
//! token with a sandboxed one before its first instruction runs. The
//! container's identifier is derived from a name; nothing is written to the
//! registry or to the user's profile.
use std::{
    ffi::{OsStr, c_void},
    io,
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr::{null, null_mut},
    time::Duration,
};

type Handle = *mut c_void;
#[repr(C)]
struct StartupInfo {
    size: u32,
    reserved: *mut u16,
    desktop: *mut u16,
    title: *mut u16,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    columns: u32,
    rows: u32,
    fill: u32,
    flags: u32,
    show: u16,
    reserved_size: u16,
    reserved_data: *mut u8,
    input: Handle,
    output: Handle,
    error: Handle,
}
#[repr(C)]
struct ProcessInformation {
    process: Handle,
    thread: Handle,
    process_id: u32,
    thread_id: u32,
}
#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root: Handle,
    name: *mut c_void,
    attributes: u32,
    security_descriptor: *mut c_void,
    quality_of_service: *mut c_void,
}
/// The token a process runs with, for `NtSetInformationProcess`.
#[repr(C)]
struct AccessToken {
    token: Handle,
    thread: Handle,
}
#[link(name = "userenv")]
unsafe extern "system" {
    fn DeriveAppContainerSidFromAppContainerName(name: *const u16, sid: *mut *mut c_void) -> i32;
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn FreeSid(sid: *mut c_void) -> *mut c_void;
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    fn GetTokenInformation(
        token: Handle,
        class: u32,
        information: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateLowBoxToken(
        token: *mut Handle,
        existing: Handle,
        access: u32,
        attributes: *mut ObjectAttributes,
        package: *mut c_void,
        capability_count: u32,
        capabilities: *mut c_void,
        handle_count: u32,
        handles: *mut Handle,
    ) -> i32;
    fn NtSetInformationProcess(
        process: Handle,
        class: u32,
        information: *mut c_void,
        length: u32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateProcessW(
        application: *const u16,
        command: *mut u16,
        process: *const c_void,
        thread: *const c_void,
        inherit: i32,
        flags: u32,
        environment: *const c_void,
        directory: *const u16,
        startup: *const StartupInfo,
        information: *mut ProcessInformation,
    ) -> i32;
    fn GetExitCodeProcess(process: Handle, code: *mut u32) -> i32;
    fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    fn TerminateProcess(process: Handle, code: u32) -> i32;
    fn ResumeThread(thread: Handle) -> u32;
    fn CloseHandle(handle: Handle) -> i32;
    fn SetErrorMode(mode: u32) -> u32;
}
/// The program gets no console: a sandboxed one could not open it.
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_SUSPENDED: u32 = 0x0000_0004;
const TOKEN_ALL_ACCESS: u32 = 0x000f_01ff;
/// `ProcessAccessToken`.
const ACCESS_TOKEN: u32 = 9;

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(Some(0)).collect()
}
fn status(code: i32) -> io::Result<()> {
    if code >= 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(code) } as i32,
        ))
    }
}

/// Whether the process behind `process` runs in an AppContainer. The handle
/// needs the right to query limited information.
pub fn contained(process: Handle) -> io::Result<bool> {
    let mut token = null_mut();
    // TOKEN_QUERY
    if unsafe { OpenProcessToken(process, 8, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let (mut container, mut length) = (0u32, 0u32);
    // TokenIsAppContainer
    let read =
        unsafe { GetTokenInformation(token, 29, (&raw mut container).cast(), 4, &mut length) };
    let error = io::Error::last_os_error();
    unsafe { CloseHandle(token) };
    if read == 0 {
        return Err(error);
    }
    Ok(container != 0)
}

/// Puts a suspended process that has not run yet into an AppContainer.
fn contain(process: &ProcessInformation) -> io::Result<()> {
    let name = wide(OsStr::new("kanade-ime.engine-tests"));
    let mut container = null_mut();
    if unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut container) } != 0 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut own = null_mut();
    let mut sandboxed = null_mut();
    let result = (|| unsafe {
        if OpenProcessToken(process.process, TOKEN_ALL_ACCESS, &mut own) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut attributes: ObjectAttributes = std::mem::zeroed();
        attributes.length = std::mem::size_of::<ObjectAttributes>() as u32;
        // A token made from the process's own one may replace it without a
        // privilege. It has no capabilities: a container with none is the
        // most restricted host a text service runs in.
        status(NtCreateLowBoxToken(
            &mut sandboxed,
            own,
            TOKEN_ALL_ACCESS,
            &mut attributes,
            container,
            0,
            null_mut(),
            0,
            null_mut(),
        ))?;
        let mut token = AccessToken {
            token: sandboxed,
            thread: process.thread,
        };
        status(NtSetInformationProcess(
            process.process,
            ACCESS_TOKEN,
            (&raw mut token).cast(),
            std::mem::size_of::<AccessToken>() as u32,
        ))
    })();
    unsafe {
        for handle in [own, sandboxed] {
            if !handle.is_null() {
                CloseHandle(handle);
            }
        }
        FreeSid(container);
    }
    result
}

/// Runs `program` with `arguments` in an AppContainer and returns its exit
/// code, or `None` when it was ended after `limit`.
pub fn run(program: &Path, arguments: &[&OsStr], limit: Duration) -> io::Result<Option<u32>> {
    // No argument here holds a quotation mark or ends in a backslash.
    let mut line = format!("\"{}\"", program.display());
    for argument in arguments {
        line.push_str(&format!(" \"{}\"", argument.to_string_lossy()));
    }
    let mut line = wide(OsStr::new(&line));
    let application = wide(program.as_os_str());
    // A folder every sandboxed program may enter.
    let directory = wide(OsStr::new(
        &std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()),
    ));
    let mut startup: StartupInfo = unsafe { std::mem::zeroed() };
    startup.size = std::mem::size_of::<StartupInfo>() as u32;
    let mut information: ProcessInformation = unsafe { std::mem::zeroed() };
    let started = unsafe {
        // A program that cannot load in the sandbox must end with a status,
        // not with a message box on the desktop. The mode is inherited.
        SetErrorMode(0x8003);
        CreateProcessW(
            application.as_ptr(),
            line.as_mut_ptr(),
            null(),
            null(),
            0,
            DETACHED_PROCESS | CREATE_SUSPENDED,
            null(),
            directory.as_ptr(),
            &startup,
            &mut information,
        )
    };
    if started == 0 {
        return Err(io::Error::last_os_error());
    }
    // A test that believes its program is sandboxed must not run one that
    // is not: check before the first instruction of the program.
    let sandboxed = contain(&information).and_then(|()| contained(information.process));
    if !matches!(sandboxed, Ok(true)) {
        unsafe {
            TerminateProcess(information.process, 1);
            CloseHandle(information.thread);
            CloseHandle(information.process);
        }
        return Err(sandboxed
            .err()
            .unwrap_or_else(|| io::ErrorKind::PermissionDenied.into()));
    }
    let code = unsafe {
        ResumeThread(information.thread);
        let ended = WaitForSingleObject(information.process, limit.as_millis() as u32) == 0;
        if !ended {
            TerminateProcess(information.process, 1);
            WaitForSingleObject(information.process, 10_000);
        }
        let mut code = 0;
        GetExitCodeProcess(information.process, &mut code);
        CloseHandle(information.thread);
        CloseHandle(information.process);
        ended.then_some(code)
    };
    Ok(code)
}
