use kanade_indexer::{Catalog, Registry};
use std::{path::Path, process::Command};

fn run(data: &Path, analyzer: &Path, args: &[&str]) {
    let out = Command::new(env!("CARGO_BIN_EXE_kanade-indexer"))
        .env("KANADE_DATA_DIR", data)
        .env("KANADE_ASTRO_SIGHT", analyzer)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
#[ignore = "requires extended astro-sight; set KANADE_ASTRO_SIGHT"]
fn approved_external_project_uses_word_readings_and_override_precedence() {
    let analyzer = std::env::var_os("KANADE_ASTRO_SIGHT").expect("set KANADE_ASTRO_SIGHT");
    let analyzer = Path::new(&analyzer);
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let data = temp.path().join("data");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("sample.rs"), "pub struct CacheRequest;\npub struct DatabaseConnection;\npub struct HTTPClient;\npub struct Utf8;\npub struct JsonParser;\n").unwrap();
    run(
        &data,
        analyzer,
        &[
            "approve",
            "sample",
            root.to_str().unwrap(),
            "com.example.Editor",
        ],
    );
    let projects = data.join("kanade/projects");
    let mut registry = Registry::load(&projects).unwrap();
    registry.projects[0]
        .readings
        .insert("CacheRequest".into(), "キャッシュヨウキュウ".into());
    registry.save(&projects).unwrap();
    std::fs::write(root.join("baseline.rs"), "pub struct SettingsRegistry;\n").unwrap();
    // With no complete base, a requested incremental update must inventory all files.
    run(&data, analyzer, &["index", "sample", "sample.rs"]);
    let catalog = Catalog::load(&projects).unwrap();
    assert_eq!(
        catalog.lookup("com.example.Editor", "セッティングレジストリー"),
        ["SettingsRegistry"]
    );
    for (value, reading) in [
        ("CacheRequest", "キャッシュヨウキュウ"),
        ("DatabaseConnection", "データベースコネクション"),
        ("HTTPClient", "エイチティーティーピークライアント"),
        ("Utf8", "ユーティーエフエイト"),
        ("JsonParser", "ジェイソンパーサー"),
    ] {
        assert_eq!(catalog.lookup("com.example.Editor", reading), [value]);
        assert!(catalog.lookup("com.example.Other", reading).is_empty());
    }
    std::fs::remove_file(root.join("baseline.rs")).unwrap();
    run(&data, analyzer, &["index", "sample", "baseline.rs"]);
    std::fs::write(root.join("extra.rs"), "pub struct SettingsRegistry;\n").unwrap();
    run(&data, analyzer, &["index", "sample", "extra.rs"]);
    let catalog = Catalog::load(&projects).unwrap();
    assert_eq!(
        catalog.lookup("com.example.Editor", "セッティングレジストリー"),
        ["SettingsRegistry"]
    );
    assert_eq!(
        catalog.lookup("com.example.Editor", "キャッシュヨウキュウ"),
        ["CacheRequest"]
    );
    std::fs::remove_file(root.join("extra.rs")).unwrap();
    run(&data, analyzer, &["index", "sample", "extra.rs"]);
    assert!(
        Catalog::load(&projects)
            .unwrap()
            .lookup("com.example.Editor", "セッティングレジストリー")
            .is_empty()
    );
    let pointer = projects.join("sample/current.json");
    let before = std::fs::read(&pointer).unwrap();
    let failed = Command::new(env!("CARGO_BIN_EXE_kanade-indexer"))
        .env("KANADE_DATA_DIR", &data)
        .env("KANADE_ASTRO_SIGHT", temp.path().join("missing-analyzer"))
        .args(["index", "sample"])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert_eq!(std::fs::read(pointer).unwrap(), before);
    run(&data, analyzer, &["remove", "sample"]);
    assert!(
        Catalog::load(&projects)
            .unwrap()
            .lookup("com.example.Editor", "キャッシュヨウキュウ")
            .is_empty()
    );
}

#[test]
#[ignore = "requires extended astro-sight; make project-contract"]
fn reapproval_never_uses_orphaned_terms_and_missing_base_triggers_full_scan() {
    let analyzer = std::env::var_os("KANADE_ASTRO_SIGHT").expect("set KANADE_ASTRO_SIGHT");
    let analyzer = Path::new(&analyzer);
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    let data = temp.path().join("data");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    std::fs::write(first.join("old.rs"), "pub struct CacheRequest;\n").unwrap();
    std::fs::write(second.join("one.rs"), "pub struct JsonParser;\n").unwrap();
    std::fs::write(second.join("two.rs"), "pub struct SettingsRegistry;\n").unwrap();
    run(
        &data,
        analyzer,
        &[
            "approve",
            "sample",
            first.to_str().unwrap(),
            "com.example.Editor",
        ],
    );
    run(&data, analyzer, &["index", "sample"]);
    let projects = data.join("kanade/projects");
    let mut registry = Registry::load(&projects).unwrap();
    let old_approval = registry.projects[0].approval;
    // Simulate a crash after durable revocation but before generation cleanup.
    registry.projects.clear();
    registry.save(&projects).unwrap();
    run(
        &data,
        analyzer,
        &[
            "approve",
            "sample",
            second.to_str().unwrap(),
            "com.example.Editor",
        ],
    );
    assert_ne!(
        Registry::load(&projects).unwrap().projects[0].approval,
        old_approval
    );
    assert!(
        Catalog::load(&projects)
            .unwrap()
            .lookup("com.example.Editor", "キャッシュリクエスト")
            .is_empty()
    );
    run(&data, analyzer, &["index", "sample", "one.rs"]);
    let catalog = Catalog::load(&projects).unwrap();
    assert_eq!(
        catalog.lookup("com.example.Editor", "ジェイソンパーサー"),
        ["JsonParser"]
    );
    assert_eq!(
        catalog.lookup("com.example.Editor", "セッティングレジストリー"),
        ["SettingsRegistry"]
    );
    let pointer = projects.join("sample/current.json");
    std::fs::write(&pointer, "{").unwrap();
    run(&data, analyzer, &["index", "sample", "one.rs"]);
    assert_eq!(
        Catalog::load(&projects)
            .unwrap()
            .lookup("com.example.Editor", "セッティングレジストリー"),
        ["SettingsRegistry"]
    );
}
