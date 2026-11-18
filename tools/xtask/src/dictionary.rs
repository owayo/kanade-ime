use super::*;
pub fn fetch(root: &Path) -> Result<()> {
    let data = PathBuf::from(
        std::env::var_os("KANADE_DATA_DIR")
            .context("set KANADE_DATA_DIR to the application data directory")?,
    )
    .join("dict");
    fs::create_dir_all(&data)?;
    let lock: toml::Value = toml::from_str(&fs::read_to_string(
        root.join("locks/dictionary.lock.toml"),
    )?)?;
    let lock = &lock["dictionary"];
    for (name, url, expected) in [
        ("ipadic.hsd", "url", "sha256"),
        ("THIRD_PARTY_LICENSES.md", "notices_url", "notices_sha256"),
    ] {
        let target = data.join(name);
        let expected = lock[expected]
            .as_str()
            .context("dictionary checksum missing")?;
        if target.is_file() && hash(&target)? == expected {
            continue;
        }
        let temp = target.with_extension("download");
        run(Command::new("curl")
            .args([
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--proto",
                "=https",
                "--max-time",
                "120",
                "--max-filesize",
                "16000000",
                "--output",
            ])
            .arg(&temp)
            .arg(lock[url].as_str().context("dictionary URL missing")?))?;
        if hash(&temp)? != expected {
            let _ = fs::remove_file(temp);
            bail!("dictionary checksum mismatch");
        }
        fs::rename(temp, target)?;
    }
    println!("dictionary and license notices verified");
    Ok(())
}
