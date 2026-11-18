use super::*;

pub fn package(root: &Path, binary: &Path, resources: &Path) -> Result<()> {
    let config: toml::Value = toml::from_str(&fs::read_to_string(root.join("mise.toml"))?)?;
    let rev = config["tools"]["cargo:owayo/astro-sight"]["version"]
        .as_str()
        .and_then(|version| version.strip_prefix("rev:"))
        .context("analyzer must pin a commit")?;
    let source = std::env::var_os("KANADE_ANALYZER_SOURCE")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("artifacts/analyzer-source"));
    if !source.exists() {
        fs::create_dir_all(source.parent().context("analyzer source parent")?)?;
        run(Command::new("git")
            .args([
                "clone",
                "--no-checkout",
                "https://github.com/owayo/astro-sight.git",
            ])
            .arg(external_tool_path(&source)?))?;
        run(Command::new("git")
            .args(["checkout", "--detach", rev])
            .current_dir(&source))?;
    }
    ensure!(
        output(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&source)
        )? == rev,
        "analyzer source differs from pin"
    );
    ensure!(
        output(
            Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&source)
        )?
        .is_empty(),
        "analyzer source must be clean"
    );
    let tool = PathBuf::from(output(
        Command::new("mise")
            .args(["which", "astro-sight"])
            .current_dir(root),
    )?);
    let manifest: toml::Value = toml::from_str(&fs::read_to_string(source.join("Cargo.toml"))?)?;
    let version = manifest["package"]["version"]
        .as_str()
        .context("analyzer version missing")?;
    ensure!(
        output(Command::new(&tool).arg("--version"))? == format!("astro-sight {version}"),
        "installed analyzer version differs from pin"
    );
    fs::copy(&tool, binary)?;
    notices::collect_analyzer(&source, &resources.join("ANALYZER_LICENSES.md"))?;
    fs::write(
        resources.join("project-analyzer.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "revision":rev, "version":version, "sha256":hash(binary)?
        }))?,
    )?;
    Ok(())
}
