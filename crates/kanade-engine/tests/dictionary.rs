#![cfg(unix)]
use kanade_ipc::{unix::DeadlineStream, *};
use serde_json::{Value, json};
use std::{
    io,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Server {
    child: Child,
    temp: tempfile::TempDir,
}

#[test]
#[ignore = "requires the locked worker and a debug engine; mise run contract"]
fn learning_exclusions_migrate_and_round_trip_without_accepting_legacy_edits() {
    let temp = tempfile::Builder::new()
        .prefix("kanade-")
        .tempdir_in("/tmp")
        .unwrap();
    let directory = temp.path().join("data/kanade");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("settings.toml"),
        "learning_apps = [\"com.apple.TextEdit\"]\nfont_size = 24.0\n",
    )
    .unwrap();
    let server = Server::start(temp);
    let settings = server
        .manage(manage::Action::GetSettings, Value::Null, 0, 0)
        .unwrap();
    assert_eq!(settings["learning_excluded_apps"], json!([]));
    assert!(settings.get("learning_apps").is_none());
    assert_eq!(settings["font_size"], 24.0);
    #[allow(deprecated)]
    let legacy = server.manage(
        manage::Action::SetLearningApps,
        json!({"previous":[],"apps":["com.example.Editor"]}),
        0,
        0,
    );
    assert!(legacy.is_err());
    let edit = json!({"previous":[],"apps":["com.example.Secret"]});
    let settings = server
        .manage(manage::Action::SetLearningExclusions, edit.clone(), 0, 0)
        .unwrap();
    assert_eq!(
        settings["learning_excluded_apps"],
        json!(["com.example.Secret"])
    );
    assert_eq!(settings["font_size"], 24.0);
    assert!(
        server
            .manage(manage::Action::SetLearningExclusions, edit, 0, 0)
            .is_err()
    );
    let saved = kanade_engine::config::Config::load(&directory.join("settings.toml")).unwrap();
    assert!(!saved.learning_allowed("com.example.Secret"));
    assert!(saved.learning_allowed("com.example.Editor"));
    let settings = server
        .manage(
            manage::Action::SetLearningExclusions,
            json!({"previous":["com.example.Secret"],"apps":[]}),
            0,
            0,
        )
        .unwrap();
    assert_eq!(settings["learning_excluded_apps"], json!([]));
}

impl Server {
    fn start(temp: tempfile::TempDir) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_kanade-engine"))
            .env("TMPDIR", temp.path())
            .env("KANADE_DATA_DIR", temp.path().join("data"))
            .env("KANADE_MOZC_WORKER", kanade_mozc::default_worker())
            .env("KANADE_ALLOW_UNSIGNED", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut server = Self { child, temp };
        let deadline = Instant::now() + Duration::from_secs(30);
        while !server.socket().exists() {
            assert!(Instant::now() < deadline);
            assert!(server.child.try_wait().unwrap().is_none());
            std::thread::sleep(Duration::from_millis(10));
        }
        server
    }
    fn socket(&self) -> std::path::PathBuf {
        self.temp.path().join("kanade-ime/engine.sock")
    }
    fn profile(&self) -> std::path::PathBuf {
        self.temp.path().join("data/mozc")
    }
    fn manage(
        &self,
        action: manage::Action,
        value: Value,
        job_id: u64,
        record_id: u64,
    ) -> io::Result<Value> {
        let mut input = Input::connect(&self.socket());
        send(
            &mut input.stream,
            &Request {
                body: Some(request::Body::Manage(Manage {
                    action: action as i32,
                    json: serde_json::to_string(&value)?,
                    job_id,
                    record_id,
                    ..Default::default()
                })),
            },
        )?;
        let response: Response = receive(&mut input.stream)?;
        let Some(response::Body::Management(response)) = response.body else {
            return Err(io::ErrorKind::InvalidData.into());
        };
        Ok(serde_json::from_str(&response.json)?)
    }
    fn submit(&self, edit: Value) -> u64 {
        self.manage(manage::Action::EditMozcDictionary, edit, 0, 0)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }
    fn status(&self, id: u64) -> Value {
        self.manage(manage::Action::DictionaryJobStatus, Value::Null, id, 0)
            .unwrap()[0]
            .clone()
    }
    fn wait(&self, id: u64, expected: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let status = self.status(id);
            if status["state"] == expected {
                return status;
            }
            assert!(
                !matches!(
                    status["state"].as_str(),
                    Some("ready" | "failed" | "cancelled")
                ),
                "unexpected job outcome"
            );
            assert!(
                Instant::now() < deadline,
                "job did not reach expected state"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn project_management_rejects_stale_edits_and_revokes_displayed_candidates() {
    for operation in [
        json!({"kind":"set_enabled","enabled":false}),
        json!({"kind":"set_apps","apps":["com.example.Other"]}),
    ] {
        check_project_access_revocation(operation);
    }
}
fn check_project_access_revocation(operation: Value) {
    use kanade_indexer::{Generation, Project, Registry, Term, write_json};
    let temp = tempfile::Builder::new()
        .prefix("kanade-projects-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = temp.path().join("source");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("source.rs"), "pub struct SampleRequest;").unwrap();
    let data = temp.path().join("data/kanade/projects");
    let mut registry = Registry::default();
    registry.projects.push(Project {
        id: "sample".into(),
        approval: 91,
        root: root.canonicalize().unwrap(),
        apps: vec!["com.apple.TextEdit".into()],
        enabled: true,
        monitoring: false,
        directory_binding: None,
        readings: Default::default(),
    });
    registry.save(&data).unwrap();
    let folder = data.join("sample");
    write_json(
        &folder.join("generation-1.json"),
        &Generation {
            generation: 1,
            approval: 91,
            files: std::collections::BTreeMap::from([(
                "source.rs".into(),
                vec![Term {
                    reading: "ガクシュウ".into(),
                    value: "SampleRequest".into(),
                }],
            )]),
        },
    )
    .unwrap();
    write_json(&folder.join("current.json"), &1u64).unwrap();
    let server = Server::start(temp);
    let mut settings = serde_json::to_value(kanade_engine::config::Config::default()).unwrap();
    settings["project_dictionary"] = json!(true);
    server
        .manage(manage::Action::SetSettings, settings, 0, 0)
        .unwrap();
    let listing = server
        .manage(manage::Action::ListProjects, Value::Null, 0, 0)
        .unwrap();
    assert_eq!(listing["projects"][0]["apps"], 1);
    assert_eq!(listing["projects"][0]["readings"], 0);
    let query = json!({"revision":listing["revision"],"id":"sample","approval":91});
    let scope = server
        .manage(manage::Action::ListProjects, query.clone(), 0, 0)
        .unwrap();
    assert_eq!(scope["apps"], json!(["com.apple.TextEdit"]));
    assert!(scope.get("readings").is_none());
    let mut input = Input::connect(&server.socket());
    for ch in "gakushuu".chars() {
        input.key(&ch.to_string(), SpecialKey::None, Privacy::Private);
        input.ack();
    }
    let converted = input.key("", SpecialKey::Space, Privacy::Private);
    input.ack();
    let item = converted
        .candidates
        .as_ref()
        .unwrap()
        .items
        .iter()
        .find(|c| c.source == CandidateSource::Project as i32 && c.value == "SampleRequest")
        .unwrap()
        .clone();
    let proposal =
        json!({"revision":listing["revision"],"id":"sample","approval":91,"operation":operation});
    let disabled = server
        .manage(manage::Action::EditProject, proposal.clone(), 0, 0)
        .unwrap();
    assert_eq!(disabled["result"], "applied");
    assert_eq!(disabled["reloaded"], true);
    let scope = server
        .manage(
            manage::Action::ListProjects,
            json!({"revision":disabled["listing"]["revision"],"id":"sample","approval":91}),
            0,
            0,
        )
        .unwrap();
    if operation["kind"] == "set_apps" {
        assert_eq!(scope["apps"], json!(["com.example.Other"]));
        assert_eq!(disabled["listing"]["projects"][0]["enabled"], true);
    } else {
        assert_eq!(disabled["listing"]["projects"][0]["enabled"], false);
    }
    assert!(
        server
            .manage(manage::Action::ListProjects, query, 0, 0)
            .is_err()
    );
    let stale = server
        .manage(manage::Action::EditProject, proposal, 0, 0)
        .unwrap();
    assert_eq!(stale["result"], "stale");
    input.seq += 1;
    send(
        &mut input.stream,
        &Request {
            body: Some(request::Body::Select(SelectRequest {
                revision: Some(input.revision),
                event_seq: input.seq,
                handle: item.handle,
                submit: true,
            })),
        },
    )
    .unwrap();
    let response: Response = receive(&mut input.stream).unwrap();
    assert!(
        matches!(response.body, Some(response::Body::Failure(Failure {code})) if code == failure::Code::Stale as i32)
    );
    let mut fresh = Input::connect(&server.socket());
    for ch in "gakushuu".chars() {
        fresh.key(&ch.to_string(), SpecialKey::None, Privacy::Private);
        fresh.ack();
    }
    let converted = fresh.key("", SpecialKey::Space, Privacy::Private);
    fresh.ack();
    assert!(
        !converted
            .candidates
            .unwrap()
            .items
            .iter()
            .any(|candidate| candidate.source == CandidateSource::Project as i32)
    );
    let revoked = server.manage(manage::Action::EditProject, json!({"revision":disabled["listing"]["revision"],"id":"sample","approval":91,"operation":{"kind":"revoke"}}), 0, 0).unwrap();
    assert_eq!(revoked["result"], "applied");
    assert_eq!(revoked["cleanup_complete"], true);
    assert!(
        revoked["listing"]["projects"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(root.join("source.rs").is_file());
    assert!(!folder.exists());
}
struct Input {
    stream: DeadlineStream,
    revision: Revision,
    seq: u64,
}

#[test]
#[ignore = "requires the pinned analyzer and locked worker; set KANADE_ASTRO_SIGHT"]
fn project_approval_and_rebuild_do_not_end_an_active_composition() {
    let temp = temp();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("one.rs"), "pub struct CacheRequest;\n").unwrap();
    let mut server = Server::start(temp);
    let mut settings = serde_json::to_value(kanade_engine::config::Config::default()).unwrap();
    settings["project_dictionary"] = json!(true);
    server
        .manage(manage::Action::SetSettings, settings, 0, 0)
        .unwrap();
    let mut input = Input::connect(&server.socket());
    let connection = input.revision.connection;
    for ch in "ka".chars() {
        assert!(
            input
                .key(&ch.to_string(), SpecialKey::None, Privacy::Private)
                .commit
                .is_none()
        );
        input.ack();
    }
    let preview = server
        .manage(
            manage::Action::PrepareProject,
            json!({"revision":0,"id":"sample","root":source,"apps":["com.apple.TextEdit"]}),
            0,
            0,
        )
        .unwrap();
    assert!(
        server
            .manage(manage::Action::ListProjects, Value::Null, 0, 0)
            .unwrap()["projects"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let request = json!({"kind":"approve","token":preview["token"]});
    let job = server
        .manage(manage::Action::StartProjectJob, request.clone(), 0, 0)
        .unwrap();
    for ch in "na".chars() {
        assert!(
            input
                .key(&ch.to_string(), SpecialKey::None, Privacy::Private)
                .commit
                .is_none()
        );
        input.ack();
    }
    let complete = wait_project_job(&server, &job);
    assert_eq!(complete["terms"], 1);
    assert_eq!(complete["reloaded"], true);
    assert_eq!(input.revision.connection, connection);
    let committed = input.key("", SpecialKey::Enter, Privacy::Private);
    input.ack();
    assert_eq!(committed.commit.unwrap().value, "かな");
    assert!(
        server
            .manage(manage::Action::StartProjectJob, request, 0, 0)
            .is_err()
    );
    let mut project_input = Input::connect(&server.socket());
    for ch in "kyasshurikuesuto".chars() {
        project_input.key(&ch.to_string(), SpecialKey::None, Privacy::Private);
        project_input.ack();
    }
    let reply = project_input.key("", SpecialKey::Space, Privacy::Private);
    project_input.ack();
    assert!(reply.preedit.as_ref().unwrap().segments.len() > 1);
    let candidate = reply
        .candidates
        .unwrap()
        .items
        .into_iter()
        .find(|candidate| {
            candidate.source == CandidateSource::Project as i32 && candidate.value == "CacheRequest"
        })
        .expect("whole identifier candidate");
    project_input.seq += 1;
    send(
        &mut project_input.stream,
        &Request {
            body: Some(request::Body::Select(SelectRequest {
                revision: Some(project_input.revision),
                event_seq: project_input.seq,
                handle: candidate.handle,
                submit: true,
            })),
        },
    )
    .unwrap();
    let response: Response = receive(&mut project_input.stream).unwrap();
    let Some(response::Body::Key(selected)) = response.body else {
        panic!("expected selected reply");
    };
    assert_eq!(selected.commit.unwrap().value, "CacheRequest");
    project_input.revision = selected.revision.unwrap();
    project_input.ack();
    std::fs::write(source.join("two.rs"), "pub struct JsonParser;\n").unwrap();
    let listing = server
        .manage(manage::Action::ListProjects, Value::Null, 0, 0)
        .unwrap();
    let rebuilt = server.manage(manage::Action::StartProjectJob, json!({"kind":"index","query":{"revision":listing["revision"],"id":"sample","approval":listing["projects"][0]["approval"]}}), 0, 0).unwrap();
    assert_eq!(wait_project_job(&server, &rebuilt)["terms"], 2);
    let listing = server
        .manage(manage::Action::ListProjects, Value::Null, 0, 0)
        .unwrap();
    let query = json!({"revision":listing["revision"],"id":"sample","approval":listing["projects"][0]["approval"]});
    let page = server
        .manage(
            manage::Action::ListProjectReadings,
            json!({"scope":query,"offset":0}),
            0,
            0,
        )
        .unwrap();
    assert_eq!(page["total"], 0);
    for ch in "ka".chars() {
        assert!(
            input
                .key(&ch.to_string(), SpecialKey::None, Privacy::Private)
                .commit
                .is_none()
        );
        input.ack();
    }
    let reading=server.manage(manage::Action::StartProjectJob,json!({"kind":"reading","edit":{"query":query,"identifier":"CacheRequest","reading":"キャッシュ"}}),0,0).unwrap();
    assert_eq!(wait_project_job(&server, &reading)["reloaded"], true);
    assert!(
        server
            .manage(
                manage::Action::ListProjectReadings,
                json!({"scope":query,"offset":0}),
                0,
                0
            )
            .is_err()
    );
    let listing = server
        .manage(manage::Action::ListProjects, Value::Null, 0, 0)
        .unwrap();
    let page=server.manage(manage::Action::ListProjectReadings,json!({"scope":{"revision":listing["revision"],"id":"sample","approval":listing["projects"][0]["approval"]},"offset":0}),0,0).unwrap();
    assert_eq!(page["entries"][0]["reading"], "キャッシュ");
    server.manage(manage::Action::EditProject,json!({"revision":listing["revision"],"id":"sample","approval":listing["projects"][0]["approval"],"operation":{"kind":"set_monitoring","enabled":true}}),0,0).unwrap();
    std::fs::write(source.join("three.rs"), "pub struct NetworkClient;\n").unwrap();
    wait_project_terms(&server, 3);
    for ch in "na".chars() {
        assert!(
            input
                .key(&ch.to_string(), SpecialKey::None, Privacy::Private)
                .commit
                .is_none()
        );
        input.ack();
    }
    assert_eq!(input.revision.connection, connection);
    let committed = input.key("", SpecialKey::Enter, Privacy::Private);
    input.ack();
    assert_eq!(committed.commit.unwrap().value, "かな");
    drop(input);
    drop(project_input);
    // Startup must observe opted-in roots without a management request.
    let retained = std::mem::replace(&mut server.temp, self::temp());
    drop(server);
    std::fs::write(source.join("four.rs"), "pub struct DatabaseConnection;\n").unwrap();
    let server = Server::start(retained);
    wait_project_terms(&server, 4);
    let report = server
        .manage(manage::Action::ProjectMonitorStatus, Value::Null, 0, 0)
        .unwrap();
    assert_eq!(report["supported"], true);
    assert_eq!(report["projects"][0]["id"], "sample");
}
fn wait_project_terms(server: &Server, expected: usize) {
    let data = server.temp.path().join("data/kanade/projects/sample");
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let count = (|| -> Option<usize> {
            let generation: u64 = kanade_indexer::read_json(&data.join("current.json")).ok()?;
            let generation: kanade_indexer::Generation =
                kanade_indexer::read_json(&data.join(format!("generation-{generation}.json")))
                    .ok()?;
            Some(generation.files.values().map(Vec::len).sum())
        })();
        if count == Some(expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "automatic index deadline exceeded"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn wait_project_job(server: &Server, job: &Value) -> Value {
    let id = job["id"].as_str().unwrap().parse().unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let mut jobs = server
            .manage(manage::Action::ProjectJobStatus, Value::Null, id, 0)
            .unwrap()
            .as_array()
            .unwrap()
            .clone();
        let status = jobs.remove(0);
        match status["state"].as_str().unwrap() {
            "ready" => return status,
            "failed" | "cancelled" => panic!("project job failed"),
            _ => {}
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
}
impl Input {
    fn connect(socket: &Path) -> Self {
        let stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
        let mut stream = DeadlineStream::new(stream, Duration::from_secs(5)).unwrap();
        send(
            &mut stream,
            &Request {
                body: Some(request::Body::Hello(Hello {
                    min_version: 2,
                    max_version: 2,
                })),
            },
        )
        .unwrap();
        let response: Response = receive(&mut stream).unwrap();
        let Some(response::Body::Welcome(welcome)) = response.body else {
            panic!("expected welcome");
        };
        Self {
            stream,
            revision: Revision {
                connection: welcome.connection,
                context: 1,
                composition: 0,
            },
            seq: 0,
        }
    }
    fn key(&mut self, text: &str, special: SpecialKey, privacy: Privacy) -> KeyReply {
        self.seq += 1;
        self.stream.begin(Duration::from_secs(5));
        send(
            &mut self.stream,
            &Request {
                body: Some(request::Body::Key(KeyRequest {
                    revision: Some(self.revision),
                    event_seq: self.seq,
                    key: Some(Key {
                        text: text.into(),
                        special: special as i32,
                        ..Default::default()
                    }),
                    context: Some(Context {
                        privacy: privacy as i32,
                        app: "com.apple.TextEdit".into(),
                        ..Default::default()
                    }),
                    passed_through: false,
                })),
            },
        )
        .unwrap();
        let response: Response = receive(&mut self.stream).unwrap();
        let Some(response::Body::Key(reply)) = response.body else {
            panic!("expected key reply");
        };
        self.revision = reply.revision.unwrap();
        reply
    }
    fn ack(&mut self) {
        send(
            &mut self.stream,
            &Request {
                body: Some(request::Body::Ack(Ack {
                    revision: Some(self.revision),
                    event_seq: self.seq,
                    applied: true,
                })),
            },
        )
        .unwrap();
    }
    fn contains(mut self, privacy: Privacy, value: &str) -> bool {
        for ch in "kanadejisho".chars() {
            self.key(&ch.to_string(), SpecialKey::None, privacy);
            self.ack();
        }
        let reply = self.key("", SpecialKey::Space, privacy);
        self.ack();
        reply
            .candidates
            .unwrap()
            .items
            .iter()
            .any(|c| c.value == value)
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        let _ = send(
            &mut self.stream,
            &Request {
                body: Some(request::Body::Close(true)),
            },
        );
    }
}
fn fixture(revision: &str) -> Value {
    json!({"operation":"upsert", "revision":revision,
        "entry":{"reading":"かなでじしょ","value":"KanadeDictionaryFixture","pos":"noun"}})
}
fn temp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("kanade-dict-")
        .tempdir_in("/tmp")
        .unwrap()
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn editing_waits_for_document_ack_and_composition_and_supports_cancellation() {
    let server = Server::start(temp());
    let original = kanade_mozc::dictionary::read(&server.profile(), 0).unwrap();
    assert_eq!(
        server
            .manage(manage::Action::DictionaryJobStatus, Value::Null, 0, 0)
            .unwrap(),
        json!([])
    );
    let mut input = Input::connect(&server.socket());
    input.key("a", SpecialKey::None, Privacy::Private);
    let first = server.submit(fixture(&original.revision));
    server.wait(first, "waiting_for_idle");
    std::thread::sleep(Duration::from_millis(2250));
    assert_eq!(server.status(first)["state"], "waiting_for_idle");
    assert_eq!(
        kanade_mozc::dictionary::read(&server.profile(), 0).unwrap(),
        original
    );
    input.ack();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(server.status(first)["state"], "waiting_for_idle");
    for _ in 0..2 {
        server
            .manage(manage::Action::CancelDictionaryJob, Value::Null, first, 0)
            .unwrap();
    }
    server.wait(first, "cancelled");
    assert_eq!(
        kanade_mozc::dictionary::read(&server.profile(), 0).unwrap(),
        original
    );
    let second = server.submit(fixture(&original.revision));
    server.wait(second, "waiting_for_idle");
    assert!(
        server
            .manage(
                manage::Action::EditMozcDictionary,
                fixture(&original.revision),
                0,
                0
            )
            .is_err()
    );
    let commit = input.key("", SpecialKey::Enter, Privacy::Private);
    assert!(commit.commit.is_some());
    assert!(commit.preedit.unwrap().segments.is_empty());
    std::thread::sleep(Duration::from_millis(2250));
    assert_eq!(server.status(second)["state"], "waiting_for_idle");
    input.ack();
    let ready = server.wait(second, "ready");
    let saved = kanade_mozc::dictionary::read(&server.profile(), 0).unwrap();
    assert_eq!(saved.total, 1);
    assert_eq!(ready["saved_revision"], saved.revision);
    assert!(!serde_json::to_string(&ready).unwrap().contains("Fixture"));
    assert!(Input::connect(&server.socket()).contains(Privacy::Normal, "KanadeDictionaryFixture"));
    assert!(
        !Input::connect(&server.socket()).contains(Privacy::Private, "KanadeDictionaryFixture")
    );
    let stale = server.submit(fixture(&original.revision));
    assert_eq!(server.wait(stale, "failed")["reason"], "revision_conflict");
    assert_eq!(
        kanade_mozc::dictionary::read(&server.profile(), 0).unwrap(),
        saved
    );
    drop(input);
    let removal = server.submit(json!({"operation":"delete","revision":saved.revision,"dictionary_index":0,"entry_index":0}));
    server.wait(removal, "ready");
    assert_eq!(
        kanade_mozc::dictionary::read(&server.profile(), 0)
            .unwrap()
            .total,
        0
    );
    assert!(!Input::connect(&server.socket()).contains(Privacy::Normal, "KanadeDictionaryFixture"));
}

#[test]
#[ignore = "requires the locked worker; make contract"]
fn explicit_promotion_uses_only_the_selected_acknowledged_record() {
    use kanade_learn::{Correction, Evidence, Store};
    let temp = temp();
    let mut learning = Store::default();
    let id = learning
        .apply(
            Correction {
                reading: "かなでじしょ".into(),
                accepted: "KanadeDictionaryFixture".into(),
                rejected: Some("辞書".into()),
                evidence: Evidence::Strong,
            },
            1,
            true,
        )
        .unwrap();
    learning
        .save(&temp.path().join("data/kanade/learning.json"))
        .unwrap();
    let server = Server::start(temp);
    let revision = kanade_mozc::dictionary::read(&server.profile(), 0)
        .unwrap()
        .revision;
    assert!(
        server
            .manage(
                manage::Action::PromoteLearning,
                json!({"revision":revision}),
                0,
                id + 1
            )
            .is_err()
    );
    let job = server
        .manage(
            manage::Action::PromoteLearning,
            json!({"revision":revision}),
            0,
            id,
        )
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    server.wait(job, "ready");
    let saved = kanade_mozc::dictionary::read(&server.profile(), 0).unwrap();
    assert_eq!(saved.total, 1);
    assert_eq!(saved.entries[0].value, "KanadeDictionaryFixture");
    server
        .manage(manage::Action::ClearLearning, Value::Null, 0, 0)
        .unwrap();
    assert_eq!(
        kanade_mozc::dictionary::read(&server.profile(), 0).unwrap(),
        saved
    );
    assert!(
        server
            .manage(
                manage::Action::PromoteLearning,
                json!({"revision":saved.revision}),
                0,
                id
            )
            .is_err()
    );
    assert!(Input::connect(&server.socket()).contains(Privacy::Normal, "KanadeDictionaryFixture"));
}
