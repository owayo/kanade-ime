// SPDX-License-Identifier: MIT
use anyhow::{Context, Result, bail};
use kanade_engine::config::Config;
use kanade_ipc::{self as ipc, manage::Action, *};
use std::time::Duration;
#[cfg(any(test, all(feature = "native", any(windows, target_os = "macos"))))]
mod edit_buffer;
pub mod models;
pub mod placement;
pub mod projects;
pub mod user_dictionary;
pub mod writing;
#[cfg(unix)]
type Stream = ipc::unix::DeadlineStream;
#[cfg(windows)]
type Stream = ipc::windows::Pipe;
/// Whether the engine could read its settings file and its learning store
/// when it started. Both are unspecified for an engine from before this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stores {
    pub settings: StoreState,
    pub learning: StoreState,
    /// A file the engine could not read, as the engine opened it, since this
    /// application may run with another data directory. Empty otherwise.
    pub settings_file: String,
    pub learning_file: String,
}
impl Stores {
    pub const UNKNOWN: Self = Self {
        settings: StoreState::Unspecified,
        learning: StoreState::Unspecified,
        settings_file: String::new(),
        learning_file: String::new(),
    };
}
pub fn manage(request: Manage) -> Result<serde_json::Value> {
    Ok(manage_with_stores(request)?.0)
}
/// Like `manage`, also reporting what the engine could read.
pub fn manage_with_stores(request: Manage) -> Result<(serde_json::Value, Stores)> {
    let timeout = Duration::from_secs(if request.action == Action::EditProject as i32 {
        15
    } else {
        5
    });
    #[cfg(unix)]
    let mut stream = Stream::connect(&ipc::unix::socket_path()?, timeout)?;
    // Like the text service, the settings application names the engine by
    // the native path beside its own, which needs no drive letter.
    #[cfg(windows)]
    let mut stream = Stream::connect(&ipc::windows::Program::Native(
        ipc::windows::native_executable()?.with_file_name("kanade-engine.exe"),
    ))?;
    stream.begin(timeout);
    ipc::send(
        &mut stream,
        &Request {
            body: Some(request::Body::Hello(Hello {
                min_version: 2,
                max_version: 2,
            })),
        },
    )?;
    let welcome: Response = ipc::receive(&mut stream)?;
    if !matches!(welcome.body, Some(response::Body::Welcome(_))) {
        bail!("engine rejected the protocol version");
    }
    stream.begin(timeout);
    ipc::send(
        &mut stream,
        &Request {
            body: Some(request::Body::Manage(request)),
        },
    )?;
    let response: Response = ipc::receive(&mut stream)?;
    let _ = ipc::send(
        &mut stream,
        &Request {
            body: Some(request::Body::Close(true)),
        },
    );
    let Some(response::Body::Management(result)) = response.body else {
        bail!("engine rejected the management request");
    };
    let state = |value| StoreState::try_from(value).unwrap_or(StoreState::Unspecified);
    let stores = Stores {
        settings: state(result.settings),
        learning: state(result.learning),
        settings_file: result.settings_file,
        learning_file: result.learning_file,
    };
    Ok((serde_json::from_str(&result.json)?, stores))
}
pub fn settings() -> Result<Config> {
    Ok(settings_with_stores()?.0)
}
/// The settings in force. While the settings file cannot be read, these keep
/// every optional feature off and the engine refuses to save them.
pub fn settings_with_stores() -> Result<(Config, Stores)> {
    let (settings, stores) = manage_with_stores(Manage {
        action: Action::GetSettings as i32,
        ..Default::default()
    })?;
    Ok((
        serde_json::from_value(settings).context("invalid settings response")?,
        stores,
    ))
}
pub fn save(settings: &Config) -> Result<()> {
    settings.validate()?;
    manage(Manage {
        action: Action::SetSettings as i32,
        json: serde_json::to_string(settings)?,
        ..Default::default()
    })?;
    Ok(())
}
#[cfg(all(feature = "native", any(windows, target_os = "macos")))]
fn save_changes(previous: &Config, settings: &Config) -> Result<()> {
    settings.validate()?;
    let previous = serde_json::to_value(previous)?;
    let mut changes: serde_json::Map<String, serde_json::Value> =
        serde_json::from_value(serde_json::to_value(settings)?)?;
    changes.retain(|key, value| previous.get(key) != Some(value));
    manage(Manage {
        action: Action::SetSettings as i32,
        json: serde_json::json!({"changes": changes}).to_string(),
        ..Default::default()
    })?;
    Ok(())
}
#[cfg(all(feature = "native", any(windows, target_os = "macos")))]
pub mod native;
