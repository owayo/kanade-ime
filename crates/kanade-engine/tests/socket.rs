#![cfg(unix)]
use kanade_ipc::{
    frontend::{Client, Document},
    unix::DeadlineStream,
    *,
};
use std::{
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[derive(Default)]
struct Editor {
    committed: String,
    preedit: String,
}
impl Document for Editor {
    fn apply(&mut self, reply: &KeyReply) -> bool {
        if let Some(commit) = &reply.commit {
            self.committed.push_str(&commit.value);
        }
        self.preedit = reply.preedit.as_ref().map(reading).unwrap_or_default();
        true
    }
    fn finish_reading(&mut self, value: &str) -> bool {
        self.committed.push_str(value);
        self.preedit.clear();
        true
    }
}
fn context() -> Context {
    Context {
        privacy: Privacy::Private as i32,
        ..Default::default()
    }
}
fn manage(socket: &std::path::Path, action: manage::Action) -> serde_json::Value {
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
    assert!(matches!(response.body, Some(response::Body::Welcome(_))));
    send(
        &mut stream,
        &Request {
            body: Some(request::Body::Manage(Manage {
                action: action as i32,
                ..Default::default()
            })),
        },
    )
    .unwrap();
    let response: Response = receive(&mut stream).unwrap();
    let Some(response::Body::Management(response)) = response.body else {
        panic!("expected management response");
    };
    send(
        &mut stream,
        &Request {
            body: Some(request::Body::Close(true)),
        },
    )
    .unwrap();
    serde_json::from_str(&response.json).unwrap()
}
#[test]
#[ignore = "requires the locked worker and a debug engine; mise run contract"]
fn socket_conversion_and_engine_loss_preserve_the_last_applied_reading() {
    let temp = tempfile::Builder::new()
        .prefix("kanade-")
        .tempdir_in("/tmp")
        .unwrap();
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_kanade-engine"))
            .env("TMPDIR", temp.path())
            .env("KANADE_DATA_DIR", temp.path().join("data"))
            .env("KANADE_MOZC_WORKER", kanade_mozc::default_worker())
            .env("KANADE_ALLOW_UNSIGNED", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let socket = temp.path().join("kanade-ime/engine.sock");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !socket.exists() {
        assert!(Instant::now() < deadline);
        assert!(server.0.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(10));
    }
    let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    let mut client = Client::connect(DeadlineStream::new(stream, KEY_TIMEOUT).unwrap()).unwrap();
    let mut editor = Editor::default();
    for action in [
        manage::Action::ClearMozcConversion,
        manage::Action::ClearMozcPrediction,
    ] {
        assert_eq!(manage(&socket, action)["cleared"], true);
    }
    for ch in "nihon".chars() {
        assert!(client.key(
            Key {
                text: ch.to_string(),
                ..Default::default()
            },
            context(),
            false,
            &mut editor
        ));
    }
    assert!(client.key(
        Key {
            special: SpecialKey::Space as i32,
            ..Default::default()
        },
        context(),
        false,
        &mut editor
    ));
    assert!(client.key(
        Key {
            special: SpecialKey::Enter as i32,
            ..Default::default()
        },
        context(),
        false,
        &mut editor
    ));
    assert_eq!(editor.committed, "日本");
    for ch in "kana".chars() {
        assert!(client.key(
            Key {
                text: ch.to_string(),
                ..Default::default()
            },
            context(),
            false,
            &mut editor
        ));
    }
    assert_eq!(editor.preedit, "かな");
    let rejected = manage(&socket, manage::Action::ClearMozcPrediction);
    assert_eq!(rejected["cleared"], false);
    assert_eq!(rejected["reason"], "composition_in_progress");
    server.0.kill().unwrap();
    server.0.wait().unwrap();
    // Losing the engine inserts nothing. The key that found it gone is used
    // up, and the composition stays until the user ends it.
    assert!(client.key(
        Key {
            text: "a".into(),
            ..Default::default()
        },
        context(),
        false,
        &mut editor,
    ));
    assert_eq!(editor.committed, "日本");
    assert_eq!(editor.preedit, "かな");
    assert!(client.is_held());
    assert!(!client.is_connected());
    assert!(client.key(
        Key {
            special: SpecialKey::Enter as i32,
            ..Default::default()
        },
        context(),
        false,
        &mut editor,
    ));
    assert_eq!(editor.committed, "日本かな");
    assert!(editor.preedit.is_empty());
}

#[test]
#[ignore = "requires the locked worker and a debug engine; mise run contract"]
fn a_dead_worker_is_restarted_when_a_new_connection_opens() {
    let temp = tempfile::Builder::new()
        .prefix("kanade-restart-")
        .tempdir_in("/tmp")
        .unwrap();
    let server = Server(
        Command::new(env!("CARGO_BIN_EXE_kanade-engine"))
            .env("TMPDIR", temp.path())
            .env("KANADE_DATA_DIR", temp.path().join("data"))
            .env("KANADE_MOZC_WORKER", kanade_mozc::default_worker())
            .env("KANADE_ALLOW_UNSIGNED", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let socket = temp.path().join("kanade-ime/engine.sock");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !socket.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let connect = || {
        let socket = std::os::unix::net::UnixStream::connect(&socket)?;
        Client::connect(DeadlineStream::new(socket, KEY_TIMEOUT)?)
    };
    drop(connect().unwrap());
    let children = Command::new("pgrep")
        .args(["-P", &server.0.id().to_string()])
        .output()
        .unwrap();
    let children = String::from_utf8(children.stdout).unwrap();
    let pid = children.lines().next().unwrap().parse::<i32>().unwrap();
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    let mut client = loop {
        if let Ok(client) = connect() {
            break client;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut editor = Editor::default();
    assert!(client.key(
        Key {
            text: "a".into(),
            ..Default::default()
        },
        context(),
        false,
        &mut editor
    ));
    assert_eq!(editor.preedit, "あ");
}

#[test]
#[ignore = "requires the locked worker and a debug engine; make contract"]
fn repeated_source_changes_commit_once_and_allow_fresh_composition() {
    let temp = tempfile::Builder::new()
        .prefix("kanade-switch-")
        .tempdir_in("/tmp")
        .unwrap();
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_kanade-engine"))
            .env("TMPDIR", temp.path())
            .env("KANADE_DATA_DIR", temp.path().join("data"))
            .env("KANADE_MOZC_WORKER", kanade_mozc::default_worker())
            .env("KANADE_ALLOW_UNSIGNED", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let socket = temp.path().join("kanade-ime/engine.sock");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !socket.exists() {
        assert!(Instant::now() < deadline);
        assert!(server.0.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(10));
    }
    for cycle in 0..24 {
        let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        let mut client =
            Client::connect(DeadlineStream::new(stream, Duration::from_secs(2)).unwrap()).unwrap();
        let mut editor = Editor::default();
        for ch in "nihongo".chars() {
            assert!(client.key(
                Key {
                    text: ch.to_string(),
                    ..Default::default()
                },
                context(),
                false,
                &mut editor,
            ));
        }
        assert_eq!(editor.preedit, "にほんご");
        if cycle % 2 == 0 {
            assert!(client.key(
                Key {
                    special: SpecialKey::Space as i32,
                    ..Default::default()
                },
                context(),
                false,
                &mut editor,
            ));
        }
        assert!(client.commit_for_source_change(context(), &mut editor));
        assert!(client.commit_for_source_change(context(), &mut editor));
        assert_eq!(
            editor.committed,
            if cycle % 2 == 0 {
                "日本語"
            } else {
                "にほんご"
            }
        );
        assert!(editor.preedit.is_empty());
    }
}

#[test]
#[ignore = "requires the locked worker and a debug engine; make contract"]
fn project_generations_reload_without_management_requests_and_revocation_fails_closed() {
    use kanade_indexer::{Generation, Project, Registry, Term, write_json};
    let temp = tempfile::Builder::new()
        .prefix("kanade-projects-")
        .tempdir_in("/tmp")
        .unwrap();
    let data = temp.path().join("data");
    let projects = data.join("kanade/projects");
    let mut registry = Registry {
        revision: 0,
        projects: vec![Project {
            id: "sample".into(),
            approval: 0,
            root: temp.path().into(),
            apps: vec!["com.example.Editor".into()],
            enabled: true,
            monitoring: false,
            directory_binding: None,
            readings: Default::default(),
        }],
    };
    registry.save(&projects).unwrap();
    let publish = |generation: u64, value: &str| {
        write_json(
            &projects.join(format!("sample/generation-{generation}.json")),
            &Generation {
                generation,
                approval: 0,
                files: std::collections::BTreeMap::from([(
                    "sample.rs".into(),
                    vec![Term {
                        reading: "かなで".into(),
                        value: value.into(),
                    }],
                )]),
            },
        )
        .unwrap();
        write_json(&projects.join("sample/current.json"), &generation).unwrap();
    };
    publish(1, "ProjectFirst");
    kanade_engine::config::Config {
        project_dictionary: true,
        learning: false,
        ..Default::default()
    }
    .save(&data.join("kanade/settings.toml"))
    .unwrap();
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_kanade-engine"))
            .env("TMPDIR", temp.path())
            .env("KANADE_DATA_DIR", &data)
            .env("KANADE_MOZC_WORKER", kanade_mozc::default_worker())
            .env("KANADE_ALLOW_UNSIGNED", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let socket = temp.path().join("kanade-ime/engine.sock");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !socket.exists() {
        assert!(Instant::now() < deadline && server.0.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(10));
    }
    struct Candidates(Vec<String>);
    impl Document for Candidates {
        fn apply(&mut self, reply: &KeyReply) -> bool {
            self.0 = reply
                .candidates
                .iter()
                .flat_map(|p| &p.items)
                .filter(|i| i.source == CandidateSource::Project as i32)
                .map(|i| i.value.clone())
                .collect();
            true
        }
        fn finish_reading(&mut self, _: &str) -> bool {
            true
        }
    }
    // This is a catalogue propagation contract. Cold session creation and test-thread
    // scheduling are not frontend latency measurements; engine key deadlines still apply.
    struct FunctionalTransport(DeadlineStream);
    impl kanade_ipc::frontend::Transport for FunctionalTransport {
        fn exchange(&mut self, request: &Request) -> std::io::Result<Response> {
            self.0.begin(Duration::from_secs(2));
            send(&mut self.0, request)?;
            receive(&mut self.0)
        }
        fn notify(&mut self, request: &Request) -> std::io::Result<()> {
            self.0.begin(Duration::from_secs(2));
            send(&mut self.0, request)
        }
    }
    let values = || {
        let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        let mut client = Client::connect(FunctionalTransport(
            DeadlineStream::new(stream, Duration::from_secs(2)).unwrap(),
        ))
        .unwrap();
        let mut document = Candidates(vec![]);
        let context = Context {
            app: "com.example.Editor".into(),
            privacy: Privacy::Private as i32,
            ..Default::default()
        };
        for ch in "kanade".chars() {
            assert!(client.key(
                Key {
                    text: ch.to_string(),
                    ..Default::default()
                },
                context.clone(),
                false,
                &mut document
            ));
        }
        assert!(client.key(
            Key {
                special: SpecialKey::Space as i32,
                ..Default::default()
            },
            context,
            false,
            &mut document
        ));
        document.0
    };
    let await_values = |expected: &[&str]| {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if values().iter().map(String::as_str).collect::<Vec<_>>() == expected {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "project reload deadline exceeded"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    await_values(&["ProjectFirst"]);
    publish(2, "ProjectSecond");
    await_values(&["ProjectSecond"]);
    std::fs::write(projects.join("registry.json"), "{").unwrap();
    await_values(&[]);
    write_json(&projects.join("registry.json"), &registry).unwrap();
    await_values(&["ProjectSecond"]);
    kanade_indexer::remove_project(&projects, "sample").unwrap();
    await_values(&[]);
}

/// An engine with the locked worker in a private directory.
struct Fixture {
    _temp: tempfile::TempDir,
    server: Server,
    socket: std::path::PathBuf,
}
impl Fixture {
    fn start() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("kanade-keep-")
            .tempdir_in("/tmp")
            .unwrap();
        let mut server = Server(
            Command::new(env!("CARGO_BIN_EXE_kanade-engine"))
                .env("TMPDIR", temp.path())
                .env("KANADE_DATA_DIR", temp.path().join("data"))
                .env("KANADE_MOZC_WORKER", kanade_mozc::default_worker())
                .env("KANADE_ALLOW_UNSIGNED", "1")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let socket = temp.path().join("kanade-ime/engine.sock");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(
                server.0.try_wait().unwrap().is_none(),
                "engine exited before accepting connections"
            );
            if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        Self {
            _temp: temp,
            server,
            socket,
        }
    }
    fn stream(&self) -> std::io::Result<DeadlineStream> {
        DeadlineStream::new(
            std::os::unix::net::UnixStream::connect(&self.socket)?,
            KEY_TIMEOUT,
        )
    }
    fn client(&self) -> Client<DeadlineStream> {
        Client::connect(self.stream().unwrap()).unwrap()
    }
    fn worker(&self) -> Option<i32> {
        let children = Command::new("pgrep")
            .args(["-P", &self.server.0.id().to_string()])
            .output()
            .unwrap();
        String::from_utf8(children.stdout)
            .unwrap()
            .lines()
            .next()
            .and_then(|pid| pid.parse().ok())
    }
    fn engine_is_running(&mut self) -> bool {
        self.server.0.try_wait().unwrap().is_none()
    }
    fn events(&self) -> String {
        std::fs::read_to_string(self._temp.path().join("data/logs/engine-events.log"))
            .unwrap_or_default()
    }
}
fn typing(client: &mut Client<DeadlineStream>, editor: &mut Editor, text: &str) {
    for ch in text.chars() {
        assert!(client.key(
            Key {
                text: ch.to_string(),
                ..Default::default()
            },
            context(),
            false,
            editor
        ));
    }
}
fn press(client: &mut Client<DeadlineStream>, editor: &mut Editor, key: SpecialKey) -> bool {
    client.key(
        Key {
            special: key as i32,
            ..Default::default()
        },
        context(),
        false,
        editor,
    )
}

#[test]
#[ignore = "requires the locked worker and a debug engine; mise run contract"]
fn a_slow_worker_keeps_its_process_its_connection_and_the_composition() {
    let mut fixture = Fixture::start();
    let mut client = fixture.client();
    let mut editor = Editor::default();
    typing(&mut client, &mut editor, "kan");
    let worker = fixture.worker().unwrap();
    // The worker stops answering for longer than the 40 ms and 50 ms limits
    // that used to end the session, then continues.
    assert_eq!(unsafe { libc::kill(worker, libc::SIGSTOP) }, 0);
    let resume = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(unsafe { libc::kill(worker, libc::SIGCONT) }, 0);
    });
    let started = Instant::now();
    typing(&mut client, &mut editor, "a");
    assert!(started.elapsed() >= Duration::from_millis(150));
    resume.join().unwrap();
    assert_eq!(editor.preedit, "かな");
    assert!(editor.committed.is_empty());
    assert!(client.is_connected() && !client.is_held());
    assert!(!client.needs_transport());
    assert_eq!(fixture.worker(), Some(worker));
    assert!(fixture.engine_is_running());
    assert!(press(&mut client, &mut editor, SpecialKey::Enter));
    assert_eq!(editor.committed, "かな");
    assert!(!fixture.events().contains("worker-"));
}

#[test]
#[ignore = "requires the locked worker and a debug engine; mise run contract"]
fn a_stuck_worker_is_bounded_and_replaced_and_the_reading_is_restored() {
    let mut fixture = Fixture::start();
    let mut client = fixture.client();
    let mut editor = Editor::default();
    // かｎ: the last letter still waits for its vowel.
    typing(&mut client, &mut editor, "kan");
    assert_eq!(editor.preedit, "かｎ");
    let worker = fixture.worker().unwrap();
    assert_eq!(unsafe { libc::kill(worker, libc::SIGSTOP) }, 0);
    let started = Instant::now();
    // The key is used up, within the frontend's limit, and inserts nothing.
    typing(&mut client, &mut editor, "x");
    assert!(started.elapsed() < KEY_TIMEOUT);
    assert!(client.is_held());
    assert!(editor.committed.is_empty());
    assert_eq!(editor.preedit, "かｎ");
    // The engine survives and starts another worker.
    let deadline = Instant::now() + Duration::from_secs(30);
    while fixture.worker().is_none_or(|pid| pid == worker) {
        assert!(Instant::now() < deadline);
        assert!(fixture.engine_is_running());
        std::thread::sleep(Duration::from_millis(20));
    }
    while client.needs_transport() {
        assert!(Instant::now() < deadline);
        if let Ok(stream) = fixture.stream() {
            let _ = client.reattach(stream);
        }
    }
    // The next key restores the reading first. The waiting ｎ still combines.
    typing(&mut client, &mut editor, "i");
    assert!(!client.is_held());
    assert_eq!(editor.preedit, "かに");
    assert!(editor.committed.is_empty());
    assert!(press(&mut client, &mut editor, SpecialKey::Enter));
    assert_eq!(editor.committed, "かに");
    let events = fixture.events();
    assert!(events.contains(" worker-call-timed-out\n"));
    assert!(events.contains(" worker-restarted\n"));
    assert!(!events.contains("fatal-"));
}

#[test]
#[ignore = "requires the locked worker and a debug engine; mise run contract"]
fn an_idle_connection_and_its_composition_stay_usable() {
    let fixture = Fixture::start();
    let mut client = fixture.client();
    let mut editor = Editor::default();
    typing(&mut client, &mut editor, "ka");
    // Quiet for longer than any frame or request limit.
    std::thread::sleep(FRAME_TIMEOUT + KEY_TIMEOUT);
    assert!(!client.needs_transport());
    typing(&mut client, &mut editor, "na");
    assert_eq!(editor.preedit, "かな");
    assert!(press(&mut client, &mut editor, SpecialKey::Enter));
    assert_eq!(editor.committed, "かな");
    assert!(client.is_connected());
}

#[test]
#[ignore = "requires the locked worker and a debug engine; mise run contract"]
fn a_frame_left_unfinished_is_closed_while_other_connections_continue() {
    use std::io::{Read, Write};
    let fixture = Fixture::start();
    let mut client = fixture.client();
    let mut editor = Editor::default();
    let mut stalled = std::os::unix::net::UnixStream::connect(&fixture.socket).unwrap();
    send(
        &mut stalled,
        &Request {
            body: Some(request::Body::Hello(Hello {
                min_version: 1,
                max_version: CURRENT_VERSION,
            })),
        },
    )
    .unwrap();
    let welcome: Response = receive(&mut stalled).unwrap();
    assert!(matches!(welcome.body, Some(response::Body::Welcome(_))));
    // Two bytes of a header, then nothing.
    stalled.write_all(&[9, 0]).unwrap();
    stalled.set_read_timeout(Some(FRAME_TIMEOUT * 3)).unwrap();
    let started = Instant::now();
    let mut byte = [0];
    assert_eq!(stalled.read(&mut byte).unwrap(), 0);
    assert!(started.elapsed() < FRAME_TIMEOUT * 2);
    typing(&mut client, &mut editor, "ka");
    assert_eq!(editor.preedit, "か");
}

#[test]
#[ignore = "requires the locked worker and a debug engine; mise run contract"]
fn shortcuts_and_a_rejected_edit_elsewhere_do_not_restart_the_worker() {
    let mut fixture = Fixture::start();
    let mut client = fixture.client();
    let mut editor = Editor::default();
    let worker = fixture.worker().unwrap();
    // A key the frontend leaves to the application while nothing is composed.
    assert!(!client.key(
        Key {
            text: "¥".into(),
            ..Default::default()
        },
        context(),
        true,
        &mut editor
    ));
    typing(&mut client, &mut editor, "ka");
    // A Command chord during composition belongs to the application.
    for text in ["s", "V"] {
        assert!(!client.key(
            Key {
                text: text.into(),
                modifiers: MOD_META,
                ..Default::default()
            },
            context(),
            true,
            &mut editor
        ));
    }
    assert_eq!(editor.preedit, "か");
    // Another frontend's document rejects an edit; only its session ends.
    struct Rejecting;
    impl Document for Rejecting {
        fn apply(&mut self, _: &KeyReply) -> bool {
            false
        }
        fn finish_reading(&mut self, _: &str) -> bool {
            false
        }
    }
    let mut other = fixture.client();
    assert!(other.key(
        Key {
            text: "a".into(),
            ..Default::default()
        },
        context(),
        false,
        &mut Rejecting
    ));
    typing(&mut client, &mut editor, "na");
    assert_eq!(editor.preedit, "かな");
    assert!(press(&mut client, &mut editor, SpecialKey::Enter));
    assert_eq!(editor.committed, "かな");
    assert!(client.is_connected() && !client.is_held());
    assert_eq!(fixture.worker(), Some(worker));
    assert!(fixture.engine_is_running());
    assert!(!fixture.events().contains("worker-"));
}
