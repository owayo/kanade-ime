use anyhow::Result;
use kanade_ipc::diagnostics::{self, Component, Event};
fn run(data: &std::path::Path) -> Result<()> {
    let worker = kanade_mozc::default_worker();
    #[cfg(unix)]
    {
        let path = kanade_ipc::unix::socket_path()?;
        kanade_engine::server::serve(&path, &worker, data)
    }
    #[cfg(windows)]
    {
        kanade_engine::server::serve(std::path::Path::new(""), &worker, data)
    }
}
fn main() -> Result<()> {
    let data = kanade_engine::config::data_directory()?;
    // An instance with its own data directory keeps its events there, away
    // from the installed engine's log.
    let logs = data.join("logs");
    diagnostics::start(Component::Engine, Some(logs.as_path()));
    let result = run(&data);
    if result.is_ok() {
        diagnostics::record(Event::EngineStopped);
    }
    // The reason for an exit is recorded where it is known; wait for it here.
    diagnostics::flush(std::time::Duration::from_secs(1));
    result
}
