use anyhow::Result;
#[cfg(not(target_os = "macos"))]
use anyhow::bail;
#[cfg(not(windows))]
use anyhow::ensure;
use std::path::Path;
#[cfg(target_os = "macos")]
use {
    anyhow::Context,
    std::{fs, path::PathBuf, process::Command},
};

#[cfg(not(windows))]
#[path = "../install.rs"]
mod install;

#[cfg(windows)]
#[path = "../install/logon_task.rs"]
mod logon_task;

#[cfg(target_os = "macos")]
fn run(command: &mut Command) -> Result<()> {
    ensure!(command.status()?.success(), "command failed");
    Ok(())
}

#[cfg(target_os = "macos")]
fn output(command: &mut Command) -> Result<String> {
    let out = command.output()?;
    ensure!(out.status.success(), "command failed");
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

#[cfg(windows)]
fn main() -> Result<()> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    match arguments.as_slice() {
        [op, engine] if op == "register-logon-task" => {
            logon_task::configure(Some(Path::new(engine)))
        }
        [op] if op == "remove-logon-task" => logon_task::configure(None),
        _ => bail!("usage: kanade-installer register-logon-task ENGINE | remove-logon-task"),
    }
}

#[cfg(not(windows))]
fn main() -> Result<()> {
    let arguments: Vec<_> = std::env::args().collect();
    ensure!(
        arguments.get(1).is_some_and(|a| a == "install-macos"),
        "usage: kanade-installer install-macos --bundle PATH"
    );
    ensure!(
        arguments.iter().any(|a| matches!(
            a.as_str(),
            "--bundle" | "--status" | "--input-source-helper" | "--uninstall"
        )),
        "a bundle path or an explicit maintenance command is required"
    );
    // Source-list helpers never need a checkout. The application source is an
    // explicitly supplied absolute path, validated by the shared installer.
    install::macos(Path::new(""))
}
