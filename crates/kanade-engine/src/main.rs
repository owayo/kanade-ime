//! The engine process.
//!
//! On Windows it is built for the windowed subsystem and so never owns a
//! console. A console window can be closed, and closing it ends every process
//! attached to it at once, without a record: an engine started from a sign-in
//! entry used to disappear that way.
#![cfg_attr(windows, windows_subsystem = "windows")]
use kanade_engine::exit;
use kanade_ipc::diagnostics::{self, Component, Event};
use std::process::ExitCode;

fn run(data: &std::path::Path) -> anyhow::Result<()> {
    let worker = kanade_mozc::default_worker();
    #[cfg(unix)]
    {
        let path = kanade_ipc::unix::socket_path()?;
        kanade_engine::server::serve(&path, &worker, data)
    }
    #[cfg(windows)]
    {
        let endpoint = kanade_engine::config::endpoint()
            .inspect_err(|_| diagnostics::record(Event::FatalConfiguration))?;
        kanade_engine::server::serve(std::path::Path::new(&endpoint), &worker, data)
    }
}
fn serve(data: &std::path::Path) -> u8 {
    match run(data) {
        Ok(()) => {
            diagnostics::record(Event::EngineStopped);
            exit::STOPPED
        }
        // The reason for an exit is recorded where it is known.
        Err(error) => {
            eprintln!("Error: {error:?}");
            if error.is::<kanade_engine::server::AlreadyRunning>() {
                exit::ALREADY_RUNNING
            } else {
                exit::FAILED
            }
        }
    }
}
fn main() -> ExitCode {
    let Ok(data) = kanade_engine::config::data_directory() else {
        return ExitCode::from(exit::FAILED);
    };
    // An instance with its own data directory keeps its events there, away
    // from the installed engine's log.
    let logs = data.join("logs");
    diagnostics::start(Component::Engine, Some(logs.as_path()));
    diagnostics::record_panics();
    let code = match std::env::args_os().nth(1) {
        None => serve(&data),
        #[cfg(windows)]
        Some(mode) if mode == "--supervise" => kanade_engine::supervisor::run(),
        Some(_) => {
            eprintln!("usage: kanade-engine [--supervise]");
            exit::USAGE
        }
    };
    diagnostics::flush(std::time::Duration::from_secs(1));
    ExitCode::from(code)
}
