// SPDX-License-Identifier: MIT
//! Starts isolated engines and inspects their processes. Shared by the
//! Windows integration tests; nothing here touches an installed engine.
#![allow(dead_code)]
pub mod sandbox;
use kanade_ipc::{
    frontend::Client,
    windows::{Pipe, Program},
};
use std::{
    ffi::c_void,
    io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

type Handle = *mut c_void;
#[repr(C)]
struct ProcessEntry {
    size: u32,
    usage: u32,
    process: u32,
    heap: usize,
    module: u32,
    threads: u32,
    parent: u32,
    priority: i32,
    flags: u32,
    name: [u16; 260],
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, process: u32) -> Handle;
    fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry) -> i32;
    fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry) -> i32;
    fn OpenProcess(access: u32, inherit: i32, process: u32) -> Handle;
    fn TerminateProcess(process: Handle, code: u32) -> i32;
    fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    fn CloseHandle(handle: Handle) -> i32;
}
#[link(name = "user32")]
unsafe extern "system" {
    fn EnumWindows(visit: unsafe extern "system" fn(Handle, isize) -> i32, context: isize) -> i32;
    fn GetWindowThreadProcessId(window: Handle, process: *mut u32) -> u32;
    fn GetClassNameW(window: Handle, name: *mut u16, length: i32) -> i32;
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtSuspendProcess(process: Handle) -> i32;
    fn NtResumeProcess(process: Handle) -> i32;
}
const SUSPEND_RESUME: u32 = 0x0800;
const TERMINATE: u32 = 0x0001;
const SYNCHRONIZE: u32 = 0x0010_0000;

fn open(process: u32, access: u32) -> Handle {
    let handle = unsafe { OpenProcess(access, 0, process) };
    assert!(!handle.is_null(), "cannot open process {process}");
    handle
}

/// Processes named `name` whose parent is `parent`.
pub fn children(parent: u32, name: &str) -> Vec<u32> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(2, 0) };
    assert!(snapshot as isize != -1);
    let mut entry: ProcessEntry = unsafe { std::mem::zeroed() };
    entry.size = std::mem::size_of::<ProcessEntry>() as u32;
    let mut found = Vec::new();
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) };
    while more != 0 {
        let length = entry.name.iter().position(|&unit| unit == 0).unwrap_or(260);
        if entry.parent == parent
            && String::from_utf16_lossy(&entry.name[..length]).eq_ignore_ascii_case(name)
        {
            found.push(entry.process);
        }
        more = unsafe { Process32NextW(snapshot, &mut entry) };
    }
    unsafe { CloseHandle(snapshot) };
    found.sort_unstable();
    found
}
/// Console windows, shown or hidden, that belong to any of the processes. A
/// console window reports the process attached to it, or its console host.
pub fn console_windows(processes: &[u32]) -> usize {
    struct Search<'a> {
        processes: &'a [u32],
        found: usize,
    }
    unsafe extern "system" fn visit(window: Handle, context: isize) -> i32 {
        let search = unsafe { &mut *(context as *mut Search) };
        let mut process = 0;
        let mut class = [0u16; 64];
        let length = unsafe {
            GetWindowThreadProcessId(window, &mut process);
            GetClassNameW(window, class.as_mut_ptr(), class.len() as i32)
        };
        if search.processes.contains(&process)
            && String::from_utf16_lossy(&class[..length.max(0) as usize]) == "ConsoleWindowClass"
        {
            search.found += 1;
        }
        1
    }
    let mut search = Search {
        processes,
        found: 0,
    };
    unsafe { EnumWindows(visit, (&raw mut search) as isize) };
    search.found
}
pub fn terminate(process: u32) {
    let handle = open(process, TERMINATE | SYNCHRONIZE);
    unsafe {
        assert_ne!(TerminateProcess(handle, 1), 0);
        WaitForSingleObject(handle, 10_000);
        CloseHandle(handle);
    }
}
/// Stops every thread of the given processes until dropped, as a machine
/// that sleeps does.
pub struct Frozen(Vec<Handle>);
impl Frozen {
    pub fn suspend(processes: &[u32]) -> Self {
        let handles: Vec<_> = processes
            .iter()
            .map(|&process| open(process, SUSPEND_RESUME))
            .collect();
        for &handle in &handles {
            assert_eq!(unsafe { NtSuspendProcess(handle) }, 0);
        }
        Self(handles)
    }
}
impl Drop for Frozen {
    fn drop(&mut self) {
        for &handle in &self.0 {
            unsafe {
                NtResumeProcess(handle);
                CloseHandle(handle);
            }
        }
    }
}
pub fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
/// The event log as written. Its writer locks the file for each line, so a
/// read that meets the lock is tried again. A log not written yet is empty.
pub fn log(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::read_to_string(path) {
            Ok(text) => return text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return String::new(),
            Err(error) => assert!(Instant::now() < deadline, "cannot read the log: {error}"),
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
/// Lines of an event log without their timestamp and process ID.
pub fn events(path: &Path) -> Vec<String> {
    log(path)
        .lines()
        .filter_map(|line| line.splitn(3, ' ').nth(2).map(str::to_owned))
        .collect()
}
pub fn label(name: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "test-{}-{}-{name}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
pub fn engine_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_kanade-engine"))
}
/// The program that stands in for a frontend in another process. See
/// `stand_in.rs`.
#[cfg(feature = "fault-injection")]
pub fn stand_in_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_kanade-stand-in"))
}
/// An engine command on a private endpoint with its own data directory.
pub fn engine_command(label: &str, data: &Path) -> Command {
    let mut command = Command::new(engine_binary());
    command
        .env("KANADE_INSTANCE", label)
        .env("KANADE_DATA_DIR", data)
        .env("KANADE_MOZC_WORKER", kanade_mozc::default_worker())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

pub struct Engine {
    child: Child,
    label: String,
    pub endpoint: String,
    data: tempfile::TempDir,
}
impl Engine {
    pub fn start(name: &str) -> Self {
        Self::start_with(name, |_| {})
    }
    /// Starts with no console to inherit, as the system does at sign-in. A
    /// console program then gets a console window of its own.
    pub fn start_as_at_sign_in(name: &str) -> Self {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_CONSOLE
        Self::start_with(name, |command| {
            command.creation_flags(0x10);
        })
    }
    /// Starts the supervisor, which starts the engine as its child.
    pub fn start_supervised(name: &str) -> Self {
        let mut supervisor = Self::spawn_with(&label(name), |command| {
            command.arg("--supervise");
        });
        supervisor.wait_for_start(1);
        supervisor
    }
    fn start_with(name: &str, prepare: impl FnOnce(&mut Command)) -> Self {
        let mut engine = Self::spawn_with(&label(name), prepare);
        engine.wait_for_start(1);
        engine
    }
    /// Starts with these files in its data directory: a path relative to the
    /// directory and the file's contents.
    pub fn start_with_files(name: &str, files: &[(&str, &str)]) -> Self {
        let data = tempfile::tempdir().unwrap();
        for (path, contents) in files {
            let path = data.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        let mut engine = Self::spawn_in(&label(name), data, |_| {});
        engine.wait_for_start(1);
        engine
    }
    /// Starts the process without waiting for an engine to serve.
    pub fn spawn_with(label: &str, prepare: impl FnOnce(&mut Command)) -> Self {
        Self::spawn_in(label, tempfile::tempdir().unwrap(), prepare)
    }
    fn spawn_in(label: &str, data: tempfile::TempDir, prepare: impl FnOnce(&mut Command)) -> Self {
        let mut command = engine_command(label, data.path());
        prepare(&mut command);
        Self {
            child: command.spawn().unwrap(),
            endpoint: kanade_ipc::windows::instance_endpoint(label).unwrap(),
            label: label.to_owned(),
            data,
        }
    }
    pub fn label(&self) -> &str {
        &self.label
    }
    pub fn data(&self) -> &Path {
        self.data.path()
    }
    /// Ends the engine, so it can write nothing more.
    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
    /// The engines a supervisor started and has not seen end.
    pub fn supervised(&self) -> Vec<u32> {
        children(self.child.id(), "kanade-engine.exe")
    }
    /// How this process ended, once it has.
    pub fn exit_code(&mut self, limit: Duration) -> Option<i32> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    pub fn wait_for_start(&mut self, starts: usize) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while self
            .events()
            .iter()
            .filter(|event| *event == "engine-started")
            .count()
            < starts
        {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "the engine exited during startup: {:?}",
                self.events()
            );
            assert!(Instant::now() < deadline, "the engine did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    /// Ends this engine and starts another on the same endpoint and data.
    pub fn restart(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let starts = self
            .events()
            .iter()
            .filter(|event| *event == "engine-started")
            .count();
        self.child = engine_command(&self.label, self.data.path())
            .spawn()
            .unwrap();
        self.wait_for_start(starts + 1);
    }
    pub fn id(&self) -> u32 {
        self.child.id()
    }
    pub fn running(&self) -> bool {
        let handle = open(self.child.id(), SYNCHRONIZE);
        let waiting = unsafe { WaitForSingleObject(handle, 0) } == 0x102;
        unsafe { CloseHandle(handle) };
        waiting
    }
    pub fn log(&self) -> String {
        log(&self.data.path().join("logs/engine-events.log"))
    }
    pub fn events(&self) -> Vec<String> {
        events(&self.data.path().join("logs/engine-events.log"))
    }
    pub fn pipe(&self) -> io::Result<Pipe> {
        Pipe::connect_to(&self.endpoint, &Program::Win32(engine_binary()))
    }
    pub fn client(&self) -> io::Result<Client<Pipe>> {
        Client::connect(self.pipe()?)
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        // A supervisor goes first, or it would start its engine again.
        let supervised = self.supervised();
        let _ = self.child.kill();
        let _ = self.child.wait();
        for engine in supervised {
            let handle = unsafe { OpenProcess(TERMINATE | SYNCHRONIZE, 0, engine) };
            if !handle.is_null() {
                unsafe {
                    TerminateProcess(handle, 1);
                    WaitForSingleObject(handle, 10_000);
                    CloseHandle(handle);
                }
            }
        }
    }
}
