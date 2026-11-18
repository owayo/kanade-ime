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
pub fn manage(request: Manage) -> Result<serde_json::Value> {
    let timeout = Duration::from_secs(if request.action == Action::EditProject as i32 {
        15
    } else {
        5
    });
    #[cfg(unix)]
    let mut stream = Stream::connect(&ipc::unix::socket_path()?, timeout)?;
    #[cfg(windows)]
    let mut stream =
        Stream::connect(&std::env::current_exe()?.with_file_name("kanade-engine.exe"))?;
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
    Ok(serde_json::from_str(&result.json)?)
}
pub fn settings() -> Result<Config> {
    serde_json::from_value(manage(Manage {
        action: Action::GetSettings as i32,
        ..Default::default()
    })?)
    .context("invalid settings response")
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
pub mod native;
