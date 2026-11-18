use std::{
    path::Path,
    process::{Command, Output},
};

fn command(data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kanade-indexer"))
        .env("KANADE_DATA_DIR", data)
        .args(args)
        .output()
        .unwrap()
}
#[test]
fn project_management_changes_only_approved_state_and_rejects_invalid_edits() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    assert!(
        command(
            &data,
            &[
                "approve",
                "sample",
                root.to_str().unwrap(),
                "com.example.Editor"
            ]
        )
        .status
        .success()
    );
    let list = command(&data, &["list"]);
    let view: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(view[0]["id"], "sample");
    assert_eq!(view[0]["apps"], serde_json::json!(["com.example.Editor"]));
    assert_eq!(view[0]["enabled"], true);
    assert!(view[0].get("approval").is_none());
    let file = data.join("kanade/projects/registry.json");
    let original = std::fs::read(&file).unwrap();
    for args in [
        vec!["reading", "missing", "CacheRequest", "キャッシュ"],
        vec!["reading", "sample", "CacheRequest", "invalid-reading"],
        vec!["reading", "sample", "two words", "キャッシュ"],
        vec!["apps", "sample"],
        vec!["apps", "sample", ""],
        vec!["disable", "sample", "extra"],
        vec!["remove", "sample", "extra"],
    ] {
        assert!(!command(&data, &args).status.success());
        assert_eq!(std::fs::read(&file).unwrap(), original);
    }
    for args in [
        vec!["reading", "sample", "CacheRequest", "キャッシュヨウキュウ"],
        vec![
            "apps",
            "sample",
            "com.example.Terminal",
            "com.example.Editor",
        ],
        vec!["disable", "sample"],
    ] {
        assert!(command(&data, &args).status.success());
    }
    let projects = data.join("kanade/projects");
    let registry = kanade_indexer::Registry::load(&projects).unwrap();
    let project = &registry.projects[0];
    assert!(!project.enabled);
    assert_eq!(project.apps, ["com.example.Terminal", "com.example.Editor"]);
    assert_eq!(project.readings["CacheRequest"], "キャッシュヨウキュウ");
    assert!(command(&data, &["enable", "sample"]).status.success());
    assert!(
        command(&data, &["clear-reading", "sample", "CacheRequest"])
            .status
            .success()
    );
    let project = kanade_indexer::Registry::load(&projects)
        .unwrap()
        .projects
        .remove(0);
    assert!(project.enabled && project.readings.is_empty());
    assert!(command(&data, &["remove", "sample"]).status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&command(&data, &["list"]).stdout).unwrap(),
        serde_json::json!([])
    );
}
