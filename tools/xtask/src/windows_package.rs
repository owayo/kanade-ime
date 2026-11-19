use super::*;

// This stages an MSI payload. Registration is performed by the MSI transaction,
// never during a Cargo build or from DllMain.
pub fn stage(root: &Path) -> Result<()> {
    ensure!(cfg!(windows), "Windows staging requires a Windows host");
    version::check(root)?;
    verify(root)?;
    let stage = root.join("artifacts/windows/payload");
    if stage.exists() {
        fs::remove_dir_all(&stage)?;
    }
    fs::create_dir_all(stage.join("x86"))?;
    let source = root.join("target/x86_64-pc-windows-msvc/release");
    for name in [
        "kanade-engine.exe",
        "kanade-ui.exe",
        "kanade-indexer.exe",
        "kanade-llm-worker.exe",
        "kanade_tip.dll",
    ] {
        fs::copy(source.join(name), stage.join(name))?;
    }
    fs::copy(
        root.join("target/i686-pc-windows-msvc/release/kanade_tip.dll"),
        stage.join("x86/kanade_tip.dll"),
    )?;
    fs::copy(
        root.join("artifacts/mozc/kanade-mozc-worker.exe"),
        stage.join("kanade-mozc-worker.exe"),
    )?;
    fs::copy(
        root.join("artifacts/mozc/manifest.json"),
        stage.join("manifest.json"),
    )?;
    analyzer::package(root, &stage.join("kanade-project-analyzer.exe"), &stage)?;
    fs::copy(notices::collect(root)?, stage.join("CARGO_LICENSES.md"))?;
    for (source, name) in [
        ("LICENSE", "LICENSE"),
        (
            "crates/kanade-engine/data/SudachiDict-LEGAL.txt",
            "SudachiDict-LEGAL.txt",
        ),
        ("crates/kanade-ui/LICENSE-APACHE", "LICENSE-APACHE.ui"),
        ("docs/third-party.md", "THIRD_PARTY.md"),
        ("third_party/mozc/LICENSE", "LICENSE.mozc"),
        (
            "third_party/mozc/src/data/installer/credits_en.html",
            "credits_en.html",
        ),
        ("third_party/hasami/LICENSE", "LICENSE.hasami"),
        (
            "third_party/hasami/LICENSE-APACHE-2.0",
            "LICENSE-APACHE-2.0.hasami",
        ),
        (
            "third_party/hasami/THIRD_PARTY_LICENSES.md",
            "LICENSES.hasami-data.md",
        ),
        (
            "third_party/hasami/src/sentence/builtin_exceptions.NOTICE",
            "NOTICE.hasami-exceptions",
        ),
    ] {
        fs::copy(root.join(source), stage.join(name))?;
    }
    fs::create_dir_all(stage.join("emoji-notices"))?;
    for entry in fs::read_dir(root.join("crates/kanade-emoji/data/notices"))? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            fs::copy(
                entry.path(),
                stage.join("emoji-notices").join(entry.file_name()),
            )?;
        }
    }
    println!("Windows MSI payload staged");
    Ok(())
}
