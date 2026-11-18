#![cfg(target_os = "macos")]

use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

struct Fixture(PathBuf);
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "kanade-installer-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::create_dir(path.join("methods")).unwrap();
        Self(path)
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kanade-installer"));
        command
            .arg("install-macos")
            .arg("--dry-run")
            .env("HOME", &self.0)
            .env("KANADE_INSTALL_DIR", self.0.join("methods"))
            .env("KANADE_DATA_DIR", self.0.join("data"));
        command
    }
    fn bundle(&self, id: &str) -> PathBuf {
        let path = self.0.join("methods/Kanade.app");
        fs::create_dir_all(path.join("Contents")).unwrap();
        fs::write(path.join("Contents/Info.plist"), format!("<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>{id}</string></dict></plist>")).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn archive_installer_needs_an_absolute_bundle_path() {
    let fixture = Fixture::new();
    let result = fixture
        .command()
        .args(["--bundle", "relative/Kanade.app"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!fixture.0.join("methods/Kanade.app").exists());
    assert!(!fixture.0.join("Library/LaunchAgents").exists());
}

#[test]
fn uninstall_preflight_preserves_the_app_and_personal_data() {
    let fixture = Fixture::new();
    let bundle = fixture.bundle("dev.kanade.inputmethod.Kanade");
    fs::create_dir(fixture.0.join("data")).unwrap();
    let data = fixture.0.join("data/settings.toml");
    fs::write(&data, "keep=true\n").unwrap();
    let result = fixture.command().arg("--uninstall").output().unwrap();
    assert!(result.status.success());
    assert!(bundle.join("Contents/Info.plist").is_file());
    assert_eq!(fs::read_to_string(data).unwrap(), "keep=true\n");
    assert!(!fixture.0.join("Library/LaunchAgents").exists());
}

#[test]
fn uninstall_preserves_a_different_application() {
    let fixture = Fixture::new();
    let bundle = fixture.bundle("com.example.Other");
    let metadata = fs::read(bundle.join("Contents/Info.plist")).unwrap();
    let result = fixture.command().arg("--uninstall").output().unwrap();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .contains("the bundle belongs to a different application")
    );
    assert_eq!(
        fs::read(bundle.join("Contents/Info.plist")).unwrap(),
        metadata
    );
}

#[test]
fn archive_install_refuses_a_different_application_before_replacement() {
    let fixture = Fixture::new();
    let bundle = fixture.bundle("com.example.Other");
    let metadata = fs::read(bundle.join("Contents/Info.plist")).unwrap();
    let result = fixture
        .command()
        .arg("--bundle")
        .arg(&bundle)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .contains("the source bundle belongs to a different application")
    );
    assert_eq!(
        fs::read(bundle.join("Contents/Info.plist")).unwrap(),
        metadata
    );
    assert!(!fixture.0.join("Library/LaunchAgents").exists());
}

#[test]
fn uninstall_refuses_an_agent_bound_to_another_installation() {
    let fixture = Fixture::new();
    let bundle = fixture.bundle("dev.kanade.inputmethod.Kanade");
    let agents = fixture.0.join("Library/LaunchAgents");
    fs::create_dir_all(&agents).unwrap();
    let path = agents.join("dev.kanade.engine.plist");
    let metadata = b"<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>ProgramArguments</key><array><string>/Applications/Other.app/Contents/MacOS/engine</string></array></dict></plist>";
    fs::write(&path, metadata).unwrap();
    let result = fixture.command().arg("--uninstall").output().unwrap();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .contains("the LaunchAgent belongs to another installation")
    );
    assert!(bundle.is_dir());
    assert_eq!(fs::read(path).unwrap(), metadata);
}
