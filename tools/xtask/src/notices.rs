use super::*;
pub fn collect(root: &Path, destination: &Path) -> Result<()> {
    let packages = cargo_packages(root, Some("kanade-ui/native,kanade-llm/llama"))?;
    let mut versions = packages.iter().filter(|p| p["name"] == "hasami");
    let hasami = versions
        .next()
        .context("hasami is missing from the locked Cargo graph")?;
    ensure!(
        versions.next().is_none(),
        "multiple hasami packages in the locked Cargo graph"
    );
    let source = Path::new(
        hasami["manifest_path"]
            .as_str()
            .context("missing hasami manifest")?,
    )
    .parent()
    .context("missing hasami source directory")?;
    fs::create_dir_all(destination)?;
    for (path, name) in [
        ("LICENSE", "LICENSE.hasami"),
        ("LICENSE-APACHE-2.0", "LICENSE-APACHE-2.0.hasami"),
        ("THIRD_PARTY_LICENSES.md", "LICENSES.hasami-data.md"),
        (
            "src/sentence/builtin_exceptions.NOTICE",
            "NOTICE.hasami-exceptions",
        ),
    ] {
        fs::copy(source.join(path), destination.join(name))
            .with_context(|| format!("copy hasami notice {path}"))?;
    }
    collect_graph(&packages, false, &destination.join("CARGO_LICENSES.md"))?;
    Ok(())
}
pub fn collect_analyzer(root: &Path, destination: &Path) -> Result<PathBuf> {
    collect_graph(&cargo_packages(root, None)?, true, destination)
}
fn cargo_packages(root: &Path, features: Option<&str>) -> Result<Vec<serde_json::Value>> {
    let target = format!(
        "{}-{}",
        std::env::consts::ARCH,
        if cfg!(target_os = "macos") {
            "apple-darwin"
        } else if cfg!(windows) {
            "pc-windows-msvc"
        } else {
            "unknown-linux-gnu"
        }
    );
    let mut command = Command::new("cargo");
    command
        .args([
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--filter-platform",
            &target,
        ])
        .current_dir(root);
    if let Some(features) = features {
        command.args(["--features", features]);
    }
    let data = output(&mut command)?;
    let mut metadata: serde_json::Value = serde_json::from_str(&data)?;
    Ok(serde_json::from_value(metadata["packages"].take())?)
}
fn collect_graph(
    packages: &[serde_json::Value],
    include_local: bool,
    destination: &Path,
) -> Result<PathBuf> {
    let mut text = String::from(
        "# Cargo dependency notices\n\nGenerated from the locked dependency graph. Each package retains its stated license.\n\n",
    );
    let mut sorted: Vec<_> = packages
        .iter()
        .filter(|p| include_local || p["source"].is_string())
        .collect();
    sorted.sort_by_key(|p| (p["name"].as_str(), p["version"].as_str()));
    for package in sorted {
        let name = package["name"].as_str().context("package name missing")?;
        let version = package["version"]
            .as_str()
            .context("package version missing")?;
        let expression = package["license"]
            .as_str()
            .unwrap_or("See upstream license file");
        text.push_str(&format!("## {name} {version}\n\nLicense: {expression}\n\n"));
        let folder = Path::new(
            package["manifest_path"]
                .as_str()
                .context("manifest missing")?,
        )
        .parent()
        .context("package directory missing")?;
        let mut files = vec![];
        for entry in fs::read_dir(folder)? {
            let path = entry?.path();
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_uppercase();
            if path.is_file()
                && ["LICENSE", "LICENCE", "COPYING", "UNLICENSE"]
                    .iter()
                    .any(|p| name.starts_with(p))
            {
                files.push(path);
            }
        }
        if let Some(file) = package["license_file"].as_str() {
            files.push(folder.join(file));
        }
        if name == "llama-cpp-sys-2" {
            // The binding crate also embeds the native inference runtime.
            ensure!(
                folder.join("llama.cpp/LICENSE").is_file(),
                "native runtime license missing"
            );
            for relative in [
                "llama.cpp/LICENSE",
                "llama.cpp/vendor/cpp-httplib/LICENSE",
                "llama.cpp/vendor/hash/xxhash/LICENSE",
                "llama.cpp/vendor/hash/sha256/LICENSE",
                "llama.cpp/vendor/hash/rotate-bits/LICENSE.md",
            ] {
                let license = folder.join(relative);
                if license.is_file() {
                    files.push(license);
                }
            }
        }
        // Workspace packages sometimes keep their license in the parent directory.
        if files.is_empty() {
            for ancestor in folder.ancestors().skip(1).take(2) {
                for name in ["LICENSE.txt", "LICENSE-MIT", "LICENSE-APACHE", "LICENSE"] {
                    let path = ancestor.join(name);
                    if path.is_file() {
                        files.push(path);
                    }
                }
                if !files.is_empty() {
                    break;
                }
            }
        }
        files.sort();
        files.dedup();
        for path in files {
            if let Ok(content) = fs::read_to_string(&path) {
                text.push_str(&format!(
                    "### {}\n\n```text\n{content}\n```\n\n",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
        }
    }
    let path = destination.to_owned();
    fs::create_dir_all(path.parent().context("notices directory")?)?;
    fs::write(&path, text)?;
    Ok(path)
}
