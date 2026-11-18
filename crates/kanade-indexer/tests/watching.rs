use kanade_indexer::Catalog;
use std::{
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread::JoinHandle,
    time::{Duration, Instant},
};

struct Watch {
    child: Child,
    notices: Receiver<String>,
    readers: Vec<JoinHandle<()>>,
}
impl Watch {
    fn start(data: &Path, analyzer: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kanade-indexer"))
            .env("KANADE_DATA_DIR", data)
            .env("KANADE_ASTRO_SIGHT", analyzer)
            .args(["watch", "sample"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (sender, notices) = mpsc::channel();
        let mut readers = vec![];
        let outputs: Vec<Box<dyn std::io::Read + Send>> = vec![
            Box::new(child.stdout.take().unwrap()),
            Box::new(child.stderr.take().unwrap()),
        ];
        for output in outputs {
            let sender = sender.clone();
            readers.push(std::thread::spawn(move || {
                for line in BufReader::new(output).lines().map_while(Result::ok) {
                    let _ = sender.send(line);
                }
            }));
        }
        Self {
            child,
            notices,
            readers,
        }
    }
    fn notice(&self, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let line = self
                .notices
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("watch notification deadline exceeded");
            if line == expected {
                return;
            }
        }
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for reader in self.readers.drain(..) {
            reader.join().unwrap();
        }
    }
}
fn operation(data: &Path, args: &[&str]) {
    let result = Command::new(env!("CARGO_BIN_EXE_kanade-indexer"))
        .env("KANADE_DATA_DIR", data)
        .args(args)
        .output()
        .unwrap();
    assert!(result.status.success(), "project operation failed");
}

#[test]
#[ignore = "requires native filesystem events and extended astro-sight; make project-contract"]
fn watch_preserves_exclusions_and_generations_and_stops_on_revocation() {
    let analyzer = std::env::var_os("KANADE_ASTRO_SIGHT").expect("set KANADE_ASTRO_SIGHT");
    let analyzer = Path::new(&analyzer);
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let data = temp.path().join("data");
    std::fs::create_dir(&root).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
    std::fs::write(root.join("ignored.rs"), "pub struct HiddenRequest;\n").unwrap();
    std::fs::write(root.join("main.rs"), "pub struct CacheRequest;\n").unwrap();
    operation(
        &data,
        &[
            "approve",
            "sample",
            root.to_str().unwrap(),
            "com.example.Editor",
        ],
    );
    let watch = Watch::start(&data, analyzer);
    watch.notice("watching approved project");
    let projects = data.join("kanade/projects");
    assert_eq!(
        Catalog::load(&projects)
            .unwrap()
            .lookup("com.example.Editor", "キャッシュリクエスト"),
        ["CacheRequest"]
    );
    assert!(
        Catalog::load(&projects)
            .unwrap()
            .lookup("com.example.Editor", "ヒドゥンリクエスト")
            .is_empty()
    );
    let current: u64 = kanade_indexer::read_json(&projects.join("sample/current.json")).unwrap();
    let generation: kanade_indexer::Generation =
        kanade_indexer::read_json(&projects.join(format!("sample/generation-{current}.json")))
            .unwrap();
    assert!(!generation.files.contains_key("ignored.rs"));
    std::fs::write(root.join(".gitignore"), "").unwrap();
    watch.notice("project dictionary updated");
    let current: u64 = kanade_indexer::read_json(&projects.join("sample/current.json")).unwrap();
    let generation: kanade_indexer::Generation =
        kanade_indexer::read_json(&projects.join(format!("sample/generation-{current}.json")))
            .unwrap();
    assert!(generation.files.contains_key("ignored.rs"));
    std::fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
    watch.notice("project dictionary updated");

    std::fs::create_dir(root.join("module")).unwrap();
    std::fs::write(
        root.join("module/new.rs"),
        "pub struct DatabaseConnection;\n",
    )
    .unwrap();
    watch.notice("project dictionary updated");
    assert_eq!(
        Catalog::load(&projects)
            .unwrap()
            .lookup("com.example.Editor", "データベースコネクション"),
        ["DatabaseConnection"]
    );
    std::fs::rename(root.join("module"), root.join("renamed")).unwrap();
    watch.notice("project dictionary updated");
    std::fs::remove_dir_all(root.join("renamed")).unwrap();
    watch.notice("project dictionary updated");
    assert!(
        Catalog::load(&projects)
            .unwrap()
            .lookup("com.example.Editor", "データベースコネクション")
            .is_empty()
    );

    let pointer = projects.join("sample/current.json");
    let previous = std::fs::read(&pointer).unwrap();
    std::fs::write(root.join("main.rs"), "pub struct Broken {\n").unwrap();
    watch.notice("project update failed; previous generation retained");
    assert_eq!(std::fs::read(&pointer).unwrap(), previous);
    std::fs::write(root.join("main.rs"), "pub struct JsonParser;\n").unwrap();
    watch.notice("project dictionary updated");
    assert_eq!(
        Catalog::load(&projects)
            .unwrap()
            .lookup("com.example.Editor", "ジェイソンパーサー"),
        ["JsonParser"]
    );
    assert!(
        Catalog::load(&projects)
            .unwrap()
            .lookup("com.example.Editor", "キャッシュリクエスト")
            .is_empty()
    );

    operation(
        &data,
        &["reading", "sample", "JsonParser", "ユーザーシテイ"],
    );
    watch.notice("project dictionary updated");
    assert_eq!(
        Catalog::load(&projects)
            .unwrap()
            .lookup("com.example.Editor", "ユーザーシテイ"),
        ["JsonParser"]
    );
    operation(&data, &["remove", "sample"]);
    watch.notice("project watch stopped: approval or root changed");
    assert!(!projects.join("sample").exists());
    assert!(
        Catalog::load(&projects)
            .unwrap()
            .lookup("com.example.Editor", "ユーザーシテイ")
            .is_empty()
    );
}

#[test]
#[ignore = "requires native filesystem events and extended astro-sight; make project-contract"]
fn disabling_a_project_stops_monitoring_and_preserves_its_generation() {
    let analyzer = std::env::var_os("KANADE_ASTRO_SIGHT").expect("set KANADE_ASTRO_SIGHT");
    let analyzer = Path::new(&analyzer);
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let data = temp.path().join("data");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("main.rs"), "pub struct CacheRequest;\n").unwrap();
    operation(
        &data,
        &[
            "approve",
            "sample",
            root.to_str().unwrap(),
            "com.example.Editor",
        ],
    );
    let mut watch = Watch::start(&data, analyzer);
    watch.notice("watching approved project");
    let projects = data.join("kanade/projects");
    let pointer = projects.join("sample/current.json");
    let before = std::fs::read(&pointer).unwrap();
    operation(&data, &["disable", "sample"]);
    watch.notice("project watch stopped: approval or root changed");
    assert!(watch.child.wait().unwrap().success());
    assert_eq!(std::fs::read(&pointer).unwrap(), before);
    assert!(
        Catalog::load(&projects)
            .unwrap()
            .lookup("com.example.Editor", "キャッシュリクエスト")
            .is_empty()
    );
    operation(&data, &["enable", "sample"]);
    assert_eq!(
        Catalog::load(&projects)
            .unwrap()
            .lookup("com.example.Editor", "キャッシュリクエスト"),
        ["CacheRequest"]
    );
    let restarted = Watch::start(&data, analyzer);
    restarted.notice("watching approved project");
}
