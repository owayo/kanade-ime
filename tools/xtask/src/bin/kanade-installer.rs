#[cfg(not(target_os = "macos"))]
use anyhow::bail;
use anyhow::{Result, ensure};
use std::path::Path;
#[cfg(target_os = "macos")]
use {
    anyhow::Context,
    std::{fs, path::PathBuf, process::Command},
};

#[path = "../install.rs"]
mod install;

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
