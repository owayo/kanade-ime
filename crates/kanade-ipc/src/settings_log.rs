//! Bounded settings lifecycle diagnostics. Events cannot contain user text.
use std::{
    fs::OpenOptions,
    io::Write,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub enum Component {
    InputMethod,
    Settings,
}
pub enum Event {
    CommandReceived,
    UnrecognizedCommand,
    ControllerDispatched,
    Requested,
    LaunchFailed,
    LaunchedActive,
    LaunchedBackground,
    Started,
    Reopened,
    WindowRequested,
    WindowFailed,
    WindowActivated,
    WindowDeactivated,
    WindowRestored,
    WindowRelocated,
    WindowVisible,
    WindowOccluded,
    WindowOffSpace,
    LoadFailed,
    Loaded,
}
impl Event {
    fn code(self) -> &'static str {
        match self {
            Self::CommandReceived => "command-received",
            Self::UnrecognizedCommand => "unrecognized-command",
            Self::ControllerDispatched => "controller-dispatched",
            Self::Requested => "open-requested",
            Self::LaunchFailed => "launch-failed",
            Self::LaunchedActive => "launched-active",
            Self::LaunchedBackground => "launched-background",
            Self::Started => "started",
            Self::Reopened => "reopened",
            Self::WindowRequested => "window-requested",
            Self::WindowFailed => "window-failed",
            Self::WindowActivated => "window-activated",
            Self::WindowDeactivated => "window-deactivated",
            Self::WindowRestored => "window-restored",
            Self::WindowRelocated => "window-relocated",
            Self::WindowVisible => "window-visible",
            Self::WindowOccluded => "window-occluded",
            Self::WindowOffSpace => "window-off-space",
            Self::LoadFailed => "load-failed",
            Self::Loaded => "loaded",
        }
    }
}
pub fn record(component: Component, event: Event) {
    // This API is used only by explicit settings commands and window lifecycle,
    // never by key handling, document editing or candidate rendering.
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let directory = std::path::PathBuf::from(home).join("Library/Logs/Kanade");
    use std::os::unix::fs::DirBuilderExt;
    if std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)
        .is_err()
    {
        return;
    }
    let filename = match component {
        Component::InputMethod => "settings-imk.log",
        Component::Settings => "settings-ui.log",
    };
    let _ = append(&directory.join(filename), event);
}
fn append(path: &Path, event: Event) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .mode(0o600)
        .open(path)?;
    if file.try_lock().is_err() {
        return Ok(());
    }
    if file.metadata()?.len() >= 64 * 1024 {
        file.set_len(0)?;
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    writeln!(
        file,
        "{timestamp} pid={} {}",
        std::process::id(),
        event.code()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn truncates_old_diagnostics_and_writes_only_lifecycle_metadata() {
        let path = std::env::temp_dir().join(format!("kanade-settings-log-{}", std::process::id()));
        std::fs::write(&path, vec![b'x'; 65536]).unwrap();
        append(&path, Event::Requested).unwrap();
        let value = std::fs::read_to_string(&path).unwrap();
        assert_eq!(value.lines().count(), 1);
        assert!(value.ends_with(" open-requested\n"));
        assert!(value.len() < 128);
        std::fs::remove_file(path).unwrap();
    }
}
