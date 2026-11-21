// SPDX-License-Identifier: MIT
//! Keeps one engine running for this user and logon session.
//!
//! The engine is started at sign-in and must outlive every failure of its
//! own: a frontend that cannot reach it passes keys on as plain letters. The
//! supervisor is the same executable in another mode. It starts the engine as
//! its child, waits for it to end, records the exit code and starts it again.
//! It has no console and no window, does not read input and holds no
//! conversion state, so there is little in it that can fail.
use kanade_ipc::diagnostics::{Detail, Event};
use std::{
    ffi::c_void,
    io,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// The pause before the first restart, doubled after each further one.
const FIRST_PAUSE: Duration = Duration::from_secs(1);
const LONGEST_PAUSE: Duration = Duration::from_secs(30);
/// An engine that ran this long had started properly; its next failure is
/// not part of a series.
const SETTLED: Duration = Duration::from_secs(30);
/// How often to look again while another engine serves the endpoint.
const ELSEWHERE_PAUSE: Duration = Duration::from_secs(2);

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateMutexW(security: *const c_void, owned: i32, name: *const u16) -> *mut c_void;
    fn WaitNamedPipeW(name: *const u16, timeout: u32) -> i32;
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
/// Whether some process offers this pipe, with or without a free instance.
fn served(endpoint: &str) -> bool {
    let name = wide(endpoint);
    let free = unsafe { WaitNamedPipeW(name.as_ptr(), 1) } != 0;
    // ERROR_FILE_NOT_FOUND: no instance of this name exists.
    free || io::Error::last_os_error().raw_os_error() != Some(2)
}
/// Allows one supervisor for an endpoint. A second sign-in script, or a
/// setup run after an update, then leaves the first one in charge.
fn only_supervisor(endpoint: &str) -> bool {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in endpoint.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    // The handle is kept until the process ends. Local names are per session.
    let name = wide(&format!("Local\\Kanade.EngineWatch.{hash:016x}"));
    let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    // ERROR_ALREADY_EXISTS: another process created it first.
    !mutex.is_null() && io::Error::last_os_error().raw_os_error() != Some(183)
}

/// Runs until the image is removed. Returns the process exit code.
pub fn run() -> u8 {
    use kanade_ipc::diagnostics::{record, record_with};
    let (Ok(image), Ok(endpoint)) = (std::env::current_exe(), crate::config::endpoint()) else {
        return crate::exit::FAILED;
    };
    if !only_supervisor(&endpoint) {
        return crate::exit::STOPPED;
    }
    record(Event::WatchStarted);
    watch(
        || image.is_file(),
        || served(&endpoint),
        || engine(&image),
        std::thread::sleep,
        record_with,
    );
    record(Event::WatchStopped);
    crate::exit::STOPPED
}
/// The engine itself: the same image without a mode. It inherits the
/// environment, so an isolated instance stays isolated.
fn engine(image: &Path) -> io::Result<Child> {
    Command::new(image)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

/// The supervision loop, with everything it touches passed in.
///
/// `present` is whether there still is an engine to start, `elsewhere`
/// whether an engine this loop did not start serves the endpoint, `launch`
/// starts one, `pause` waits and `report` records. An engine that ends is
/// started again after a pause that grows while failures follow each other
/// quickly, so a persistent failure cannot spin.
pub fn watch(
    mut present: impl FnMut() -> bool,
    mut elsewhere: impl FnMut() -> bool,
    mut launch: impl FnMut() -> io::Result<Child>,
    mut pause: impl FnMut(Duration),
    mut report: impl FnMut(Event, Detail),
) {
    let mut wait = FIRST_PAUSE;
    let mut launches = 0u32;
    while present() {
        if elsewhere() {
            // An engine started by other means keeps the endpoint. Starting
            // another would only fail; take over once that one has ended.
            pause(ELSEWHERE_PAUSE);
            continue;
        }
        if launches > 0 {
            report(Event::EngineRestarted, Detail::Count(launches));
        }
        launches = launches.saturating_add(1);
        let started = Instant::now();
        match launch().and_then(|mut child| child.wait()) {
            // A Windows exit code is unsigned; a status value fills 32 bits.
            Ok(status) => report(
                Event::EngineExited,
                Detail::ExitCode(status.code().unwrap_or(-1) as u32),
            ),
            Err(_) => report(Event::EngineLaunchFailed, Detail::None),
        }
        if started.elapsed() >= SETTLED {
            wait = FIRST_PAUSE;
        }
        pause(wait);
        wait = (wait * 2).min(LONGEST_PAUSE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn exits_with(code: u32) -> io::Result<Child> {
        Command::new("cmd")
            .args(["/d", "/c", "exit", &code.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    }
    #[test]
    fn every_exit_is_recorded_with_its_code_and_followed_by_a_counted_restart() {
        let codes = [101, 1, 0xc000_013a, 0];
        let launched = Cell::new(0);
        let pauses = RefCell::new(Vec::new());
        let reports = RefCell::new(Vec::new());
        watch(
            || launched.get() < codes.len(),
            || false,
            || {
                launched.set(launched.get() + 1);
                exits_with(codes[launched.get() - 1])
            },
            |pause| pauses.borrow_mut().push(pause),
            |event, detail| reports.borrow_mut().push((event, detail)),
        );
        assert_eq!(
            *reports.borrow(),
            [
                (Event::EngineExited, Detail::ExitCode(101)),
                (Event::EngineRestarted, Detail::Count(1)),
                (Event::EngineExited, Detail::ExitCode(1)),
                (Event::EngineRestarted, Detail::Count(2)),
                // The status a closed console window leaves.
                (Event::EngineExited, Detail::ExitCode(0xc000_013a)),
                (Event::EngineRestarted, Detail::Count(3)),
                // Even an orderly exit leaves frontends without an engine.
                (Event::EngineExited, Detail::ExitCode(0)),
            ]
        );
        // Failures that follow each other quickly are spaced further apart.
        let seconds: Vec<_> = pauses.borrow().iter().map(Duration::as_secs).collect();
        assert_eq!(seconds, [1, 2, 4, 8]);
    }
    #[test]
    fn the_pause_stops_growing_and_an_engine_that_cannot_start_is_retried() {
        let attempts = Cell::new(0);
        let pauses = RefCell::new(Vec::new());
        let reports = RefCell::new(Vec::new());
        watch(
            || attempts.get() < 8,
            || false,
            || {
                attempts.set(attempts.get() + 1);
                Err(io::ErrorKind::NotFound.into())
            },
            |pause| pauses.borrow_mut().push(pause),
            |event, detail| reports.borrow_mut().push((event, detail)),
        );
        let seconds: Vec<_> = pauses.borrow().iter().map(Duration::as_secs).collect();
        assert_eq!(seconds, [1, 2, 4, 8, 16, 30, 30, 30]);
        let failed = reports
            .borrow()
            .iter()
            .filter(|report| **report == (Event::EngineLaunchFailed, Detail::None))
            .count();
        assert_eq!(failed, 8);
    }
    #[test]
    fn an_engine_started_by_other_means_is_left_alone_until_it_ends() {
        let looks = Cell::new(0);
        let launched = Cell::new(0);
        let reports = RefCell::new(Vec::new());
        watch(
            || launched.get() == 0,
            || {
                looks.set(looks.get() + 1);
                looks.get() <= 3
            },
            || {
                launched.set(1);
                exits_with(0)
            },
            |_| {},
            |event, detail| reports.borrow_mut().push((event, detail)),
        );
        // Nothing was started, counted or recorded while the other one ran.
        assert_eq!(looks.get(), 4);
        assert_eq!(
            *reports.borrow(),
            [(Event::EngineExited, Detail::ExitCode(0))]
        );
    }
    #[test]
    fn a_second_supervisor_for_one_endpoint_stands_down() {
        let endpoint = format!(r"\\.\pipe\kanade-test.{}.watch", std::process::id());
        assert!(only_supervisor(&endpoint));
        assert!(!only_supervisor(&endpoint));
        assert!(only_supervisor(&format!("{endpoint}.other")));
        assert!(!served(&endpoint));
    }
}
