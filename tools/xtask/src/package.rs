use super::*;
pub fn macos(root: &Path, debug: bool) -> Result<()> {
    version::check(root)?;
    emoji::verify(root)?;
    ensure!(cfg!(target_os = "macos"), "macOS packaging requires a Mac");
    verify(root)?;
    let mut build = Command::new("cargo");
    let mut flags = std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME") {
        if !flags.is_empty() {
            flags.push('\u{1f}');
        }
        flags.push_str(&format!(
            "--remap-path-prefix={}=/home/builder",
            Path::new(&home).display()
        ));
    }
    if !flags.is_empty() {
        flags.push('\u{1f}');
    }
    flags.push_str(&format!(
        "--remap-path-prefix={}=/workspace/kanade-ime",
        root.display()
    ));
    build.env("CARGO_ENCODED_RUSTFLAGS", flags);
    build.args([
        "build",
        "--locked",
        "-p",
        "kanade-engine",
        "-p",
        "kanade-imk",
        "-p",
        "kanade-ui",
        "-p",
        "kanade-llm",
        "-p",
        "kanade-indexer",
        "--features",
        "kanade-ui/native,kanade-llm/llama",
    ]);
    if !debug {
        build.arg("--release");
    }
    run(&mut build)?;
    let legacy = root.join("artifacts/macos");
    let output = root.join("artifacts/macos.noindex");
    if legacy.is_dir() && !output.exists() {
        #[cfg(target_os = "macos")]
        install::unregister_build_copy(&legacy.join("Kanade.app"))?;
        fs::rename(&legacy, &output)?;
    }
    let staging = root.join("artifacts/macos.noindex/Staging.app");
    if staging.is_dir() {
        fs::remove_dir_all(&staging)?;
    }
    let bundle = staging.join("Contents");
    let bin = bundle.join("MacOS");
    let resources = bundle.join("Resources");
    fs::create_dir_all(&bin)?;
    fs::create_dir_all(&resources)?;
    let source = root.join(if debug {
        "target/debug"
    } else {
        "target/release"
    });
    for name in [
        "kanade-engine",
        "kanade-imk",
        "kanade-indexer",
        "kanade-llm-worker",
    ] {
        fs::copy(source.join(name), bin.join(name))?;
    }
    analyzer::package(root, &bin.join("kanade-project-analyzer"), &resources)?;
    let settings = bundle.join("Helpers/Settings.app");
    fs::create_dir_all(settings.join("Contents/MacOS"))?;
    fs::copy(
        source.join("kanade-ui"),
        settings.join("Contents/MacOS/kanade-ui"),
    )?;
    fs::copy(
        root.join("packaging/macos/Settings.plist"),
        settings.join("Contents/Info.plist"),
    )?;
    fs::copy(
        root.join("artifacts/mozc/kanade-mozc-worker"),
        bin.join("kanade-mozc-worker"),
    )?;
    fs::copy(
        root.join("artifacts/mozc/manifest.json"),
        resources.join("manifest.json"),
    )?;
    let _ = fs::remove_file(bin.join("manifest.json"));
    fs::copy(
        root.join("packaging/macos/Info.plist"),
        bundle.join("Info.plist"),
    )?;
    fs::write(
        resources.join("kanade-build.json"),
        b"{\"registration_cli\":1}\n",
    )?;
    icons(root, &output, &resources)?;
    fs::create_dir_all(settings.join("Contents/Resources"))?;
    fs::copy(
        resources.join("Kanade.icns"),
        settings.join("Contents/Resources/Kanade.icns"),
    )?;
    for language in ["ja", "en"] {
        let localized = format!("{language}.lproj");
        fs::create_dir_all(resources.join(&localized))?;
        fs::copy(
            root.join("packaging/macos")
                .join(&localized)
                .join("InfoPlist.strings"),
            resources.join(localized).join("InfoPlist.strings"),
        )?;
    }
    fs::copy(root.join("LICENSE"), resources.join("LICENSE"))?;
    let emoji_notices = resources.join("emoji-notices");
    fs::create_dir_all(&emoji_notices)?;
    for name in [
        "emojibase-LICENSE.txt",
        "emoji-ja-LICENSE.txt",
        "emojilib-LICENSE.txt",
        "unicode-LICENSE.txt",
        "IPAdic-and-hasami-NOTICE.md",
    ] {
        fs::copy(
            root.join("crates/kanade-emoji/data/notices").join(name),
            emoji_notices.join(name),
        )?;
    }
    fs::copy(notices::collect(root)?, resources.join("CARGO_LICENSES.md"))?;
    fs::copy(
        root.join("docs/third-party.md"),
        resources.join("THIRD_PARTY.md"),
    )?;
    fs::copy(
        root.join("third_party/hasami/LICENSE"),
        resources.join("LICENSE.hasami"),
    )?;
    fs::copy(
        root.join("third_party/hasami/src/sentence/builtin_exceptions.NOTICE"),
        resources.join("NOTICE.hasami-exceptions"),
    )?;
    fs::copy(
        root.join("third_party/hasami/THIRD_PARTY_LICENSES.md"),
        resources.join("LICENSES.hasami-data.md"),
    )?;
    fs::copy(
        root.join("third_party/hasami/LICENSE-APACHE-2.0"),
        resources.join("LICENSE-APACHE-2.0.hasami"),
    )?;
    for name in ["LICENSE.mozc", "credits_en.html"] {
        fs::copy(root.join("artifacts/mozc").join(name), resources.join(name))?;
    }
    if let Ok(identity) = std::env::var("CODESIGN_IDENTITY") {
        for name in [
            "kanade-engine",
            "kanade-indexer",
            "kanade-mozc-worker",
            "kanade-llm-worker",
            "kanade-project-analyzer",
        ] {
            run(Command::new("codesign")
                .args(["--force", "--options", "runtime", "--sign", &identity])
                .arg(bin.join(name)))?;
        }
        run(Command::new("codesign")
            .args(["--force", "--options", "runtime", "--sign", &identity])
            .arg(&settings))?;
        let manifest_path = resources.join("manifest.json");
        let analyzer_path = resources.join("project-analyzer.json");
        let mut analyzer: serde_json::Value = serde_json::from_slice(&fs::read(&analyzer_path)?)?;
        analyzer["sha256"] = hash(&bin.join("kanade-project-analyzer"))?.into();
        fs::write(analyzer_path, serde_json::to_vec_pretty(&analyzer)?)?;
        let mut manifest: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
        manifest["sha256"] = hash(&bin.join("kanade-mozc-worker"))?.into();
        fs::write(manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
        run(Command::new("codesign")
            .args(["--force", "--options", "runtime", "--sign", &identity])
            .arg(bundle.parent().context("bundle")?))?;
        run(Command::new("codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(bundle.parent().context("bundle")?))?;
    }
    let target = root.join("artifacts/macos.noindex/Kanade.app");
    let previous = root.join("artifacts/macos.noindex/.Kanade.previous");
    if previous.is_dir() {
        fs::remove_dir_all(&previous)?;
    }
    if target.is_dir() {
        fs::rename(&target, &previous)?;
    }
    fs::rename(&staging, &target)?;
    if previous.is_dir() {
        fs::remove_dir_all(&previous)?;
    }
    println!("bundle prepared: artifacts/macos.noindex/Kanade.app");
    Ok(())
}

fn icons(root: &Path, output: &Path, resources: &Path) -> Result<()> {
    let assets = root.join("assets/icons");
    // The supplied mark includes outer transparent padding. Match the native
    // Japanese input icons' full 16-point footprint without changing the art.
    let input = output.join("input-menu-master.png");
    run(Command::new("sips")
        .args([
            "--cropToHeightWidth",
            "1060",
            "1060",
            "--cropOffset",
            "112",
            "97",
        ])
        .arg(assets.join("input-template.png"))
        .arg("--out")
        .arg(&input))?;
    for name in ["Kanade", "KanadeInputMenu"] {
        let iconset = output.join(format!("{name}.iconset"));
        fs::create_dir_all(&iconset)?;
        for size in [16, 32, 128, 256, 512] {
            for scale in [1, 2] {
                let pixels = size * scale;
                // Preserve the supplied 512 px artwork without inventing extra detail.
                if pixels > 512 {
                    continue;
                }
                let source = if name == "Kanade" {
                    assets.join("app.png")
                } else {
                    input.clone()
                };
                let suffix = if scale == 2 { "@2x" } else { "" };
                run(Command::new("sips")
                    .args([
                        "--resampleHeightWidth",
                        &pixels.to_string(),
                        &pixels.to_string(),
                    ])
                    .arg(source)
                    .arg("--out")
                    .arg(iconset.join(format!("icon_{size}x{size}{suffix}.png"))))?;
            }
        }
        run(Command::new("iconutil")
            .args(["--convert", "icns", "--output"])
            .arg(resources.join(format!("{name}.icns")))
            .arg(&iconset))?;
    }
    let mut representations = Vec::new();
    for (pixels, dpi) in [(16, 72), (32, 144)] {
        let image = output.join(format!("tray-{pixels}.tiff"));
        run(Command::new("sips")
            .args([
                "--resampleHeightWidth",
                &pixels.to_string(),
                &pixels.to_string(),
            ])
            .args(["--setProperty", "format", "tiff"])
            .args(["--setProperty", "dpiWidth", &dpi.to_string()])
            .args(["--setProperty", "dpiHeight", &dpi.to_string()])
            .arg(&input)
            .arg("--out")
            .arg(&image))?;
        representations.push(image);
    }
    run(Command::new("tiffutil")
        // Keep each representation's DPI: the 32 px image is still 16 points.
        .arg("-cat")
        .args(representations)
        .arg("-out")
        .arg(resources.join("KanadeInputMenu.tiff")))
}
