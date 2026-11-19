use super::*;
use std::{
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

fn rpc(
    binary: &Path,
    request: serde_json::Value,
    control: Option<&crate::workflow::Control>,
) -> Result<serde_json::Value> {
    ensure!(
        !control.is_some_and(crate::workflow::Control::cancelled),
        "index cancelled"
    );
    let deadline = Instant::now() + Duration::from_secs(35);
    // Never inherit a developer's debug logging configuration. The temporary
    // file contains fixed options only, not identifiers, paths or source text.
    let mut config = tempfile::NamedTempFile::new()?;
    config.write_all(b"debug = false\n")?;
    let mut command = Command::new(binary);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Runs only in the child, leaving engine priority unchanged.
        unsafe {
            command.pre_exec(|| {
                if libc::setpriority(libc::PRIO_PROCESS, 0, 10) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
    }
    let mut child = command
        .arg("--config")
        .arg(config.path())
        .arg("session")
        .env("RAYON_NUM_THREADS", "2")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().context("missing analysis input")?;
    let stdout = child.stdout.take().context("missing analysis output")?;
    let (tx, rx) = mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut bytes = vec![];
        let result = (|| -> std::io::Result<()> {
            // A full pipe must not hide cancellation or the exchange deadline.
            writeln!(stdin, "{request}")?;
            drop(stdin);
            stdout.take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
            Ok(())
        })();
        let _ = tx.send((result, bytes));
    });
    let response = loop {
        if control.is_some_and(crate::workflow::Control::cancelled) || Instant::now() >= deadline {
            break Err(mpsc::RecvTimeoutError::Timeout);
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            result => break result,
        }
    };
    if response.is_err() {
        let _ = child.kill();
    }
    while child.try_wait()?.is_none() {
        if control.is_some_and(crate::workflow::Control::cancelled) || Instant::now() >= deadline {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = child.wait()?;
    let _ = reader.join();
    let (result, bytes) = response.context("analysis deadline exceeded")?;
    result?;
    ensure!(
        status.success() && bytes.len() <= 8 * 1024 * 1024,
        "analysis failed or exceeded output limit"
    );
    let response: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(
        response.get("error").is_none(),
        "analysis request was rejected"
    );
    Ok(response.get("data").cloned().unwrap_or(response))
}
pub fn index(data: &Path, id: &str, binary: &Path, changed: Option<Vec<String>>) -> Result<usize> {
    index_inner(data, id, binary, changed, None, None)
}
pub(crate) fn index_bound(
    data: &Path,
    project: &Project,
    binary: &Path,
    control: &crate::workflow::Control,
) -> Result<usize> {
    index_inner(
        data,
        &project.id,
        binary,
        None,
        Some(project),
        Some(control),
    )
}
fn index_inner(
    data: &Path,
    id: &str,
    binary: &Path,
    changed: Option<Vec<String>>,
    expected: Option<&Project>,
    control: Option<&crate::workflow::Control>,
) -> Result<usize> {
    ensure!(valid_id(id), "invalid project ID");
    let _index_lock = storage_lock(data, &format!("index-{id}"))?;
    let registry = Registry::load(data)?;
    let project = registry
        .projects
        .iter()
        .find(|p| p.id == id && p.enabled)
        .context("project has not been approved")?;
    ensure!(
        expected.is_none_or(|expected| expected == project),
        "project approval changed"
    );
    let root = project.root.canonicalize()?;
    let identity = RootIdentity::capture(&root)?;
    ensure!(
        project
            .directory_binding
            .as_ref()
            .is_none_or(|binding| identity.matches_saved(binding)),
        "approved directory changed"
    );
    ensure!(
        root == project.root && root.is_dir(),
        "approved root has moved or became a symbolic link"
    );
    let folder = data.join(id);
    let previous = read_json::<u64>(&folder.join("current.json")).unwrap_or(0);
    let base = read_json::<Generation>(&folder.join(format!("generation-{previous}.json")))
        .ok()
        .filter(|g| previous != 0 && g.generation == previous && g.approval == project.approval);
    // An incremental update is meaningful only on a complete generation of this approval.
    let changed = changed.filter(|_| base.is_some());
    let report = rpc(
        binary,
        serde_json::json!({"command":"scan_symbols", "dir":root, "source_files":changed}),
        control,
    )?;
    ensure!(
        report["complete"] == true,
        "analysis was incomplete; existing generation retained"
    );
    let mut generation = if changed.is_some() {
        base.context("missing incremental base")?
    } else {
        Generation::default()
    };
    if let Some(changed) = &changed {
        for path in changed {
            generation.files.remove(&path.replace('\\', "/"));
        }
    }
    for removed in report["removed"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
    {
        generation.files.remove(removed);
    }
    let files = report["files"]
        .as_array()
        .context("invalid analysis report")?;
    let names: Vec<_> = files
        .iter()
        .flat_map(|f| f["symbols"].as_array().into_iter().flatten())
        .filter_map(|s| s["name"].as_str())
        .take(50_000)
        .map(str::to_owned)
        .collect();
    ensure!(names.len() <= 10_000, "too many identifiers");
    let parts = rpc(
        binary,
        serde_json::json!({"command":"split_identifier", "names":names}),
        control,
    )?;
    let parts = parts["parts"]
        .as_array()
        .context("identifier splitting is unavailable")?;
    ensure!(
        parts.len() == names.len(),
        "identifier result length mismatch"
    );
    let mut readings = BTreeMap::new();
    for (name, parts) in names.iter().zip(parts) {
        let reading = identifier_reading(name, parts, &project.readings);
        if let Some(reading) = reading {
            readings.insert(name.as_str(), reading);
        }
    }
    for file in files {
        let path = file["path"].as_str().context("analysis path missing")?;
        let mut terms = vec![];
        for symbol in file["symbols"].as_array().into_iter().flatten() {
            let Some(name) = symbol["name"].as_str() else {
                continue;
            };
            if let Some(reading) = readings.get(name) {
                let term = Term {
                    reading: reading.clone(),
                    value: name.to_owned(),
                };
                if valid_term(&term) && !terms.contains(&term) {
                    terms.push(term);
                }
            }
        }
        generation.files.insert(path.to_owned(), terms);
    }
    publish_controlled(data, project, &identity, previous, generation, control)
}
#[cfg(test)]
fn publish(
    data: &Path,
    project: &Project,
    identity: &RootIdentity,
    previous: u64,
    generation: Generation,
) -> Result<usize> {
    publish_controlled(data, project, identity, previous, generation, None)
}
fn publish_controlled(
    data: &Path,
    project: &Project,
    identity: &RootIdentity,
    previous: u64,
    mut generation: Generation,
    control: Option<&crate::workflow::Control>,
) -> Result<usize> {
    let _registry_lock = storage_lock(data, "registry")?;
    let approved = Registry::load(data)?;
    ensure!(
        approved.projects.iter().any(|p| p == project) && identity.matches(&project.root),
        "project approval changed during analysis; generation not published"
    );
    let folder = data.join(&project.id);
    if let Some(control) = control {
        control.publish()?;
    }
    generation.generation = previous.checked_add(1).context("generation exhausted")?;
    generation.approval = project.approval;
    let count = generation.files.values().map(Vec::len).sum();
    write_json(
        &folder.join(format!("generation-{}.json", generation.generation)),
        &generation,
    )?;
    write_json(&folder.join("current.json"), &generation.generation)?;
    // Old immutable generations stay valid until the new pointer is durable.
    for entry in std::fs::read_dir(&folder)?.flatten() {
        let name = entry.file_name();
        let old = name
            .to_str()
            .and_then(|s| s.strip_prefix("generation-"))
            .and_then(|s| s.strip_suffix(".json"))
            .and_then(|s| s.parse::<u64>().ok());
        if old.is_some_and(|old| old < previous) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(count)
}
fn identifier_reading(
    name: &str,
    parts: &serde_json::Value,
    overrides: &BTreeMap<String, String>,
) -> Option<String> {
    overrides
        .get(name)
        .cloned()
        .or_else(|| crate::readings::lookup(name).map(str::to_owned))
        .or_else(|| {
            let pieces: Option<Vec<_>> = parts
                .as_array()?
                .iter()
                .map(|part| pronounce(part.as_str()?))
                .collect();
            pieces.filter(|p| !p.is_empty()).map(|p| p.concat())
        })
}

fn pronounce(word: &str) -> Option<String> {
    if let Some(reading) = crate::readings::lookup(word) {
        return Some(reading.into());
    }
    if word
        .chars()
        .all(|c| matches!(c,'ぁ'..='ゖ'|'ァ'..='ヺ'|'ー'))
    {
        return Some(word.into());
    }
    const LETTERS: [&str; 26] = [
        "エー",
        "ビー",
        "シー",
        "ディー",
        "イー",
        "エフ",
        "ジー",
        "エイチ",
        "アイ",
        "ジェー",
        "ケー",
        "エル",
        "エム",
        "エヌ",
        "オー",
        "ピー",
        "キュー",
        "アール",
        "エス",
        "ティー",
        "ユー",
        "ブイ",
        "ダブリュー",
        "エックス",
        "ワイ",
        "ゼット",
    ];
    word.bytes()
        .map(|b| {
            b.is_ascii_alphabetic()
                .then(|| {
                    LETTERS
                        .get((b.to_ascii_lowercase() - b'a') as usize)
                        .copied()
                })
                .flatten()
        })
        .collect::<Option<Vec<_>>>()
        .map(|v| v.concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(unix)]
    fn cancellation_interrupts_an_analyzer_that_does_not_read_its_input() {
        use std::sync::Arc;
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("analyzer");
        // The child creates its own executable, avoiding inherited writable FDs
        // from other tests' concurrent spawns.
        assert!(
            Command::new("/bin/sh")
                .args([
                    "-c",
                    "printf '%s\\n' '#!/bin/sh' 'exec /bin/sleep 3' > \"$1\"; chmod 700 \"$1\"",
                    "fixture"
                ])
                .arg(&binary)
                .status()
                .unwrap()
                .success()
        );
        let control = Arc::new(crate::workflow::Control::new());
        let worker = control.clone();
        let started = Instant::now();
        let thread = std::thread::spawn(move || {
            rpc(
                &binary,
                serde_json::json!({"payload":"x".repeat(1024*1024)}),
                Some(&worker),
            )
        });
        std::thread::sleep(Duration::from_millis(100));
        assert!(control.cancel());
        assert!(thread.join().unwrap().is_err());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancellation waited for blocked input"
        );
    }
    #[test]
    fn changed_approval_cannot_publish_an_old_scan() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let root = temp.path().canonicalize().unwrap();
        let project = Project {
            id: "sample".into(),
            approval: 0,
            root: root.clone(),
            apps: vec!["com.example.Editor".into()],
            enabled: true,
            monitoring: false,
            directory_binding: None,
            readings: Default::default(),
        };
        let mut registry = Registry {
            revision: 0,
            projects: vec![project.clone()],
        };
        registry.save(&data).unwrap();
        let identity = RootIdentity::capture(&root).unwrap();
        publish(&data, &project, &identity, 0, Generation::default()).unwrap();
        let pointer = data.join("sample/current.json");
        let before = std::fs::read(&pointer).unwrap();
        registry.projects[0].enabled = false;
        registry.save(&data).unwrap();
        assert!(publish(&data, &project, &identity, 1, Generation::default()).is_err());
        assert_eq!(std::fs::read(pointer).unwrap(), before);
    }
    #[test]
    fn identifier_readings_use_words_acronyms_and_explicit_overrides() {
        let empty = BTreeMap::new();
        for (name, parts, expected) in [
            (
                "CacheRequest",
                vec!["Cache", "Request"],
                "キャッシュリクエスト",
            ),
            (
                "DatabaseConnection",
                vec!["Database", "Connection"],
                "データベースコネクション",
            ),
            (
                "HTTPClient",
                vec!["HTTP", "Client"],
                "エイチティーティーピークライアント",
            ),
            ("Utf8", vec!["Utf", "8"], "ユーティーエフエイト"),
            ("JsonParser", vec!["Json", "Parser"], "ジェイソンパーサー"),
        ] {
            assert_eq!(
                identifier_reading(name, &serde_json::json!(parts), &empty).as_deref(),
                Some(expected)
            );
        }
        let overrides = BTreeMap::from([("Utf8".into(), "ユーザーシテイ".into())]);
        assert_eq!(
            identifier_reading("Utf8", &serde_json::json!(["Utf", "8"]), &overrides).as_deref(),
            Some("ユーザーシテイ")
        );
        assert_eq!(pronounce("ZQX").as_deref(), Some("ゼットキューエックス"));
        assert_eq!(pronounce("かな").as_deref(), Some("かな"));
        assert!(pronounce("bad-value").is_none());
    }
}
