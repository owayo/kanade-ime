use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};
mod analyzer;
mod dictionary;
mod emoji;
mod install;
mod notices;
mod package;
mod protocol;
mod version;
mod windows_package;

fn prepare(root: &Path) -> Result<()> {
    let source = root.join("third_party/hasami");
    let manifest: toml::Value = toml::from_str(&fs::read_to_string(root.join("Cargo.toml"))?)?;
    let commit = manifest["workspace"]["dependencies"]["hasami"]["rev"]
        .as_str()
        .context("hasami dependency must pin a commit")?;
    ensure!(
        output(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&source)
        )? == commit,
        "hasami commit differs from lock"
    );
    ensure!(
        output(
            Command::new("git")
                .args(["status", "--porcelain", "--untracked-files=normal"])
                .current_dir(&source)
        )?
        .is_empty(),
        "hasami checkout must be unmodified"
    );
    Ok(())
}

fn run(command: &mut Command) -> Result<()> {
    ensure!(command.status()?.success(), "command failed");
    Ok(())
}
fn output(command: &mut Command) -> Result<String> {
    let out = command.output()?;
    ensure!(out.status.success(), "command failed");
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace")
}
fn hash(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let len = file.read(&mut buffer)?;
        if len == 0 {
            break;
        }
        digest.update(&buffer[..len]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
fn generate(root: &Path) -> Result<()> {
    let compiler = protoc_bin_vendored::protoc_bin_path()?;
    let src = root.join("third_party/mozc/src");
    let target = root.join("crates/kanade-mozc/src/generated");
    fs::create_dir_all(&target)?;
    let protos: Vec<_> = [
        "commands",
        "candidate_window",
        "config",
        "engine_builder",
        "user_dictionary_storage",
    ]
    .iter()
    .map(|p| src.join(format!("protocol/{p}.proto")))
    .collect();
    prost_build::Config::new()
        .protoc_executable(&compiler)
        .out_dir(&target)
        .file_descriptor_set_path(target.join("mozc-descriptor.bin"))
        .compile_protos(&protos, &[&src])?;
    let target = root.join("crates/kanade-ipc/src/generated");
    fs::create_dir_all(&target)?;
    prost_build::Config::new()
        .protoc_executable(compiler)
        .out_dir(target)
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        .compile_protos(&[root.join("proto/kanade.proto")], &[root.join("proto")])?;
    Ok(())
}
fn verify(root: &Path) -> Result<()> {
    let lock: toml::Value =
        toml::from_str(&fs::read_to_string(root.join("locks/mozc.lock.toml"))?)?;
    let src = root.join("third_party/mozc");
    let commit = output(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&src),
    )?;
    ensure!(
        lock["mozc"]["commit"].as_str() == Some(&commit),
        "Mozc commit differs from lock"
    );
    ensure!(
        output(
            Command::new("git")
                .args(["status", "--porcelain", "--untracked-files=all"])
                .current_dir(&src)
        )?
        .is_empty(),
        "Mozc working tree is modified"
    );
    for (path, expected) in lock["proto"].as_table().context("proto hashes missing")? {
        ensure!(
            Some(hash(&src.join("src").join(path))?.as_str()) == expected.as_str(),
            "proto hash mismatch: {path}"
        );
    }
    Ok(())
}
fn windows_tool_path(path: &str) -> String {
    let path = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).to_owned()
    };
    path.replace('\\', "/")
}

fn external_tool_path(path: &Path) -> Result<String> {
    let path = path.to_str().context("external tool path must be UTF-8")?;
    Ok(if cfg!(windows) {
        windows_tool_path(path)
    } else {
        path.to_owned()
    })
}

fn bazel_value_line(stdout: &str) -> Result<&str> {
    // The pinned Windows wrapper prints these diagnostics on stdout.
    let mut paths = stdout.lines().filter(|line| {
        !line.is_empty()
            && !["BAZEL_LLVM=", "BAZEL_REAL=", "BAZEL_VC="]
                .iter()
                .any(|prefix| line.starts_with(prefix))
    });
    let path = paths.next().context("Bazel did not return a value")?;
    ensure!(
        paths.next().is_none(),
        "Bazel returned ambiguous value output"
    );
    Ok(path)
}

fn bazel_output_path(
    workspace: &Path,
    version: &str,
    target: &str,
    options: &[&str],
) -> Result<PathBuf> {
    let files = output(
        Command::new("bazelisk")
            .env("USE_BAZEL_VERSION", version)
            .current_dir(workspace)
            .args(["cquery", target, "--output=files"])
            .args(options),
    )?;
    let execution_root = output(
        Command::new("bazelisk")
            .env("USE_BAZEL_VERSION", version)
            .current_dir(workspace)
            .args(["info", "execution_root"]),
    )?;
    let file = bazel_value_line(&files).context("cannot read Bazel target output")?;
    let execution_root =
        bazel_value_line(&execution_root).context("cannot read Bazel execution root")?;
    let path = PathBuf::from(execution_root).join(file);
    path.canonicalize().with_context(|| {
        format!("cannot resolve Bazel output: execution_root={execution_root:?}, file={file:?}")
    })
}

#[cfg(windows)]
fn clear_readonly(path: &Path) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    if permissions.readonly() {
        // On Windows this changes FILE_ATTRIBUTE_READONLY only.
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

// Explicit build-tool rehearsal; ordinary Cargo builds and tests never run it.
fn check_bazel_paths(root: &Path) -> Result<()> {
    let lock: toml::Value =
        toml::from_str(&fs::read_to_string(root.join("locks/mozc.lock.toml"))?)?;
    let version = lock["mozc"]["bazel"]
        .as_str()
        .context("Bazel lock missing")?;
    let fixture = root.join("artifacts/bazel path fixture");
    fs::create_dir_all(&fixture)?;
    // Reuse the unchanged upstream wrapper so Windows exercises its stdout.
    for file in [
        ".bazeliskrc",
        "bazel/bazel_wrapper/bazel.bat",
        "build_tools/vs_util.py",
    ] {
        let destination = fixture.join(file);
        fs::create_dir_all(destination.parent().context("fixture parent missing")?)?;
        fs::copy(root.join("third_party/mozc/src").join(file), destination)?;
    }
    fs::write(
        fixture.join("MODULE.bazel"),
        "module(name = \"path_fixture\")\n",
    )?;
    fs::write(fixture.join("input.txt"), b"build fixture\n")?;
    fs::write(
        fixture.join("BUILD.bazel"),
        r#"genrule(
    name = "worker",
    srcs = ["input.txt"],
    outs = ["external/+local_repository+kanade_worker/kanade-mozc-worker.exe"],
    cmd = "cat $(location input.txt) > $@",
    cmd_bat = "type \"$(location input.txt)\" > \"$@\"",
)
"#,
    )?;
    let result = (|| -> Result<()> {
        let git_source = fixture.join("git source");
        let git_clone = fixture.join("git clone");
        fs::create_dir_all(&git_source)?;
        run(Command::new("git")
            .args(["init", "--quiet"])
            .arg(external_tool_path(&git_source)?))?;
        run(Command::new("git")
            .args(["clone", "--no-checkout"])
            .arg(external_tool_path(&git_source)?)
            .arg(external_tool_path(&git_clone)?))?;
        ensure!(
            git_clone.join(".git").is_dir(),
            "Git clone fixture is missing"
        );
        println!("Git clone accepted external tool paths");
        run(Command::new("bazelisk")
            .env("USE_BAZEL_VERSION", version)
            .current_dir(&fixture)
            .args(["build", "//:worker", "--symlink_prefix=/", "--jobs=2"]))?;
        let source = bazel_output_path(&fixture, version, "//:worker", &["--symlink_prefix=/"])?;
        let stage = fixture.join("staged/artifacts/mozc");
        fs::create_dir_all(&stage)?;
        let staged = stage.join("kanade-mozc-worker.exe.next");
        fs::copy(&source, &staged)
            .with_context(|| format!("cannot copy {source:?} to {staged:?}"))?;
        ensure!(
            fs::read_to_string(&staged)?.replace("\r\n", "\n") == "build fixture\n",
            "staged fixture differs"
        );
        #[cfg(windows)]
        clear_readonly(&staged)?;
        Ok(())
    })();
    let shutdown = run(Command::new("bazelisk")
        .env("USE_BAZEL_VERSION", version)
        .current_dir(&fixture)
        .arg("shutdown"));
    result?;
    shutdown?;
    fs::remove_dir_all(fixture)?;
    println!("Bazel output staging without workspace links passed");
    Ok(())
}

fn build_worker(root: &Path) -> Result<()> {
    verify(root)?;
    let lock: toml::Value =
        toml::from_str(&fs::read_to_string(root.join("locks/mozc.lock.toml"))?)?;
    let bazel = lock["mozc"]["bazel"]
        .as_str()
        .context("Bazel lock missing")?;
    let injected = format!(
        "--inject_repository=kanade_worker={}",
        external_tool_path(&root.join("mozc-worker"))?
    );
    let src = root.join("third_party/mozc/src");
    run(Command::new("bazelisk")
        .env("USE_BAZEL_VERSION", bazel)
        .current_dir(&src)
        .args([
            "build",
            "@kanade_worker//:kanade-mozc-worker",
            &injected,
            "--check_visibility=false",
            "--compilation_mode=opt",
            "--jobs=6",
        ]))?;
    let path = bazel_output_path(
        &src,
        bazel,
        "@kanade_worker//:kanade-mozc-worker",
        &[
            &injected,
            "--check_visibility=false",
            "--compilation_mode=opt",
        ],
    )?;
    let name = if cfg!(windows) {
        "kanade-mozc-worker.exe"
    } else {
        "kanade-mozc-worker"
    };
    let target = root.join("artifacts/mozc");
    fs::create_dir_all(&target)?;
    let temporary = target.join(format!("{name}.next"));
    if temporary.exists() {
        fs::remove_file(&temporary)?;
    }
    fs::copy(&path, &temporary).with_context(|| {
        format!("cannot stage the Bazel worker output: {path:?} to {temporary:?}")
    })?;
    #[cfg(windows)]
    if target.join(name).is_file() {
        clear_readonly(&target.join(name))?;
    }
    fs::rename(temporary, target.join(name))?;
    fs::copy(
        root.join("third_party/mozc/LICENSE"),
        target.join("LICENSE.mozc"),
    )?;
    let credits = src.join("data/installer/credits_en.html");
    if credits.is_file() {
        fs::copy(credits, target.join("credits_en.html"))?;
    }
    let manifest = serde_json::json!({"worker_protocol":2, "mozc_commit":lock["mozc"]["commit"].as_str(), "tag":lock["mozc"]["tag"].as_str(), "bazel":lock["mozc"]["bazel"].as_str(), "platform":format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH), "sha256":hash(&target.join(name))?});
    fs::write(
        target.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    verify(root)?;
    println!("worker: {}", target.join(name).display());
    Ok(())
}

fn lock(root: &Path) -> Result<()> {
    let src = root.join("third_party/mozc");
    ensure!(
        output(
            Command::new("git")
                .args(["status", "--porcelain", "--untracked-files=all"])
                .current_dir(&src)
        )?
        .is_empty(),
        "cannot lock a modified Mozc working tree"
    );
    let commit = output(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&src),
    )?;
    let mut hashes = BTreeMap::new();
    for entry in fs::read_dir(src.join("src/protocol"))? {
        let path = entry?.path();
        if path.extension().is_some_and(|v| v == "proto") {
            hashes.insert(
                format!(
                    "protocol/{}",
                    path.file_name().context("filename")?.to_string_lossy()
                ),
                hash(&path)?,
            );
        }
    }
    let versions = fs::read_to_string(src.join("src/version.bzl"))?;
    let number = |name: &str| -> Result<u32> {
        let value = versions
            .lines()
            .filter_map(|line| line.split_once('='))
            .find_map(|(key, value)| (key.trim() == name).then_some(value))
            .with_context(|| format!("missing upstream version: {name}"))?;
        value
            .split('#')
            .next()
            .unwrap_or_default()
            .trim()
            .parse()
            .with_context(|| format!("upstream version must be an integer: {name}"))
    };
    let tag = format!(
        "{}.{}.{}",
        number("MAJOR")?,
        number("MINOR")?,
        number("BUILD_OSS")?
    );
    let bazel = output(
        Command::new("bazelisk")
            .arg("--version")
            .current_dir(src.join("src")),
    )?;
    let bazel = bazel_value_line(&bazel)?
        .strip_prefix("bazel ")
        .context("Bazel version missing")?;
    let value = serde_json::json!({"mozc":{"tag":tag,"commit":commit,"bazel":bazel,"engine_version":number("ENGINE_VERSION")?,"data_version":number("DATA_VERSION")?},"proto":hashes});
    fs::create_dir_all(root.join("locks"))?;
    fs::write(
        root.join("locks/mozc.lock.toml"),
        toml::to_string_pretty(&value)?,
    )?;
    Ok(())
}
fn main() -> Result<()> {
    let root = root();
    match std::env::args().nth(1).as_deref() {
        Some("version") => {
            println!("{}", version::check(&root)?);
            Ok(())
        }
        Some("bump-version") => {
            let arguments: Vec<_> = std::env::args().skip(2).collect();
            ensure!(
                arguments.is_empty() || arguments == ["--dry-run"],
                "usage: cargo xtask bump-version [--dry-run]"
            );
            version::bump(&root, !arguments.is_empty())
        }
        Some("prepare") => prepare(&root),
        Some("install-macos") => install::macos(&root),
        Some("fetch-dictionary") => dictionary::fetch(&root),
        Some("emoji-data") => emoji::generate(&root),
        Some("notices") => {
            notices::collect(&root)?;
            Ok(())
        }
        Some("proto-check") => protocol::check(
            &root,
            &PathBuf::from(
                std::env::args()
                    .nth(2)
                    .context("candidate Mozc checkout required")?,
            ),
        ),
        Some("bundle-macos") => package::macos(&root, std::env::args().any(|a| a == "--debug")),
        Some("stage-windows") => windows_package::stage(&root),
        Some("generate") => generate(&root),
        Some("lock") => lock(&root),
        Some("verify") => {
            version::check(&root)?;
            emoji::verify(&root)?;
            verify(&root)
        }
        Some("build-worker") => build_worker(&root),
        Some("check-bazel-paths") => check_bazel_paths(&root),
        Some("check") => {
            version::check(&root)?;
            emoji::verify(&root)?;
            run(Command::new("cargo").args(["fmt", "--all", "--", "--check"]))?;
            run(Command::new("cargo").args([
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ]))?;
            let mut tests = Command::new("cargo");
            tests.args(["test", "--workspace"]);
            // Integration tests also build the xtask executable. Windows holds
            // the running copy open until this command returns.
            #[cfg(windows)]
            {
                let target = std::env::var_os("CARGO_TARGET_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| root.join("target"));
                tests
                    .arg("--target-dir")
                    .arg(target.join("workspace-tests"));
            }
            run(&mut tests)?;
            verify(&root)
        }
        _ => bail!(
            "usage: cargo xtask <prepare|generate|lock|verify|build-worker|check-bazel-paths|bundle-macos|version|bump-version [--dry-run]|check>"
        ),
    }
}

#[cfg(test)]
mod worker_path_tests {
    use super::{bazel_value_line, windows_tool_path};

    #[test]
    fn upstream_windows_wrapper_diagnostics_are_not_paths() {
        let diagnostics = "BAZEL_LLVM=\r\nBAZEL_REAL=C:\\Tools\\Bazel\\bazel.exe\r\nBAZEL_VC=C:\\Program Files\\Visual Studio\\VC\r\n";
        for path in [
            "bazel-out/x64_windows-opt/bin/external/+local_repository+kanade_worker/kanade-mozc-worker.exe",
            r"C:\build cache\execroot\_main",
            "bazel 9.0.2",
        ] {
            assert_eq!(
                bazel_value_line(&format!("{diagnostics}{path}\r\n")).unwrap(),
                path
            );
            assert_eq!(bazel_value_line(path).unwrap(), path);
        }
        assert!(bazel_value_line(diagnostics).is_err());
        assert!(bazel_value_line("").is_err());
        assert!(bazel_value_line("first/path\nsecond/path").is_err());
        assert!(bazel_value_line("unexpected output\nactual/path").is_err());
    }

    #[test]
    fn canonical_windows_paths_are_portable_tool_arguments() {
        for (path, expected) in [
            (r"\\?\D:\build\ime\mozc-worker", "D:/build/ime/mozc-worker"),
            (r"D:\build folder\日本語", "D:/build folder/日本語"),
            (
                r"\\?\UNC\build-server\share\ime",
                "//build-server/share/ime",
            ),
            (r"\\build-server\share\ime", "//build-server/share/ime"),
        ] {
            assert_eq!(windows_tool_path(path), expected);
        }
    }
}
