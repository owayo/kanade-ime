//! Real socket, Mozc and model integration. All data lives in a temporary profile.
#![cfg(unix)]
use kanade_ipc::{frontend::*, unix::DeadlineStream, *};
use std::{
    path::{Path, PathBuf},
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
    edits: usize,
    surface: String,
    committed: String,
    page: CandidatePage,
}
impl Document for Editor {
    fn apply(&mut self, reply: &KeyReply) -> bool {
        self.edits += 1;
        self.surface = reply
            .preedit
            .as_ref()
            .map(|p| p.segments.iter().map(|s| s.value.as_str()).collect())
            .unwrap_or_default();
        self.page = reply.candidates.clone().unwrap_or_default();
        if let Some(commit) = &reply.commit {
            self.committed.push_str(&commit.value);
        }
        true
    }
    fn finish_reading(&mut self, _: &str) -> bool {
        panic!("the real socket must stay connected");
    }
}
fn manage(socket: &Path, action: manage::Action, value: serde_json::Value) -> serde_json::Value {
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
                json: if value.is_null() {
                    String::new()
                } else {
                    value.to_string()
                },
                ..Default::default()
            })),
        },
    )
    .unwrap();
    let response: Response = receive(&mut stream).unwrap();
    let Some(response::Body::Management(response)) = response.body else {
        panic!("expected authenticated model management response");
    };
    serde_json::from_str(&response.json).unwrap()
}

fn start_server(temp: &Path, worker: &Path) -> (Server, PathBuf) {
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_kanade-engine"))
            .env("TMPDIR", temp)
            .env("KANADE_DATA_DIR", temp.join("data"))
            .env("KANADE_MOZC_WORKER", kanade_mozc::default_worker())
            .env("KANADE_LLM_WORKER", worker)
            .env("KANADE_ALLOW_UNSIGNED", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let socket = temp.join("kanade-ime/engine.sock");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            return (server, socket);
        }
        assert!(Instant::now() < deadline, "engine socket deadline");
        assert!(server.0.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_ready(socket: &Path) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let status = manage(socket, manage::Action::LlmStatus, serde_json::Value::Null);
        assert_ne!(
            status["job"]["phase"], "failed",
            "model qualification failed: category={}, reason={}, measurement={}",
            status["job"]["error"], status["reason"], status["qualification"]
        );
        if status["ready"] == true {
            assert_eq!(status["enabled"], true);
            assert_eq!(status["installed"]["consent_current"], true);
            return status;
        }
        assert!(Instant::now() < deadline, "model qualification deadline");
        std::thread::sleep(Duration::from_millis(80));
    }
}

#[test]
#[ignore = "requires KANADE_LLM_MODEL, KANADE_LLM_WORKER and the locked Mozc worker"]
fn real_model_refines_candidates_after_ack_without_editing_the_document() {
    let model = std::env::var_os("KANADE_LLM_MODEL").expect("set KANADE_LLM_MODEL");
    let worker = std::env::var_os("KANADE_LLM_WORKER").expect("set KANADE_LLM_WORKER");
    let temp = tempfile::Builder::new()
        .prefix("kanade-llm-")
        .tempdir_in("/tmp")
        .unwrap();
    let data = temp.path().join("data");
    let worker = PathBuf::from(worker);
    let (server, socket) = start_server(temp.path(), &worker);
    use manage::Action;
    let status = manage(&socket, Action::LlmStatus, serde_json::Value::Null);
    let entry = &status["catalog"][0];
    let result = manage(
        &socket,
        Action::LlmImport,
        serde_json::json!({"import_and_activate": {
            "path": Path::new(&model), "model": entry["id"], "license": entry["license"],
            "terms_sha256": entry["terms_sha256"],
            "accept_license": true, "accept_local_processing": true
        }}),
    );
    assert_eq!(result["result"], "started");
    // A setting changed while the model is being prepared must survive
    // automatic activation. Conversely the edit never switches LLM off.
    manage(
        &socket,
        Action::SetSettings,
        serde_json::json!({"changes": {"font_size": 22.0}}),
    );
    wait_for_ready(&socket);
    let settings_path = data.join("kanade/settings.toml");
    let saved = kanade_engine::config::Config::load(&settings_path).unwrap();
    assert!(saved.llm);
    assert_eq!(saved.font_size, 22.0);
    for enabled in [false, true] {
        manage(
            &socket,
            Action::SetSettings,
            serde_json::json!({"changes": {"llm": enabled}}),
        );
        let status = manage(&socket, Action::LlmStatus, serde_json::Value::Null);
        assert_eq!(status["enabled"], enabled);
        assert_eq!(status["installed"]["consent_current"], true);
        assert_eq!(
            kanade_engine::config::Config::load(&settings_path)
                .unwrap()
                .llm,
            enabled
        );
    }
    // A shutdown between writing the consent and completing qualification
    // leaves the imported model and enabled setting, but no measurement.
    drop(server);
    let model_dir = data.join("kanade/llm");
    let consent = std::fs::read(model_dir.join("install.json")).unwrap();
    std::fs::remove_file(model_dir.join("qualification.json")).unwrap();
    let (_restarted, socket) = start_server(temp.path(), &worker);
    let recovered = wait_for_ready(&socket);
    assert_eq!(recovered["job"]["kind"], "qualify");
    assert_eq!(
        std::fs::read(model_dir.join("install.json")).unwrap(),
        consent
    );
    // This contract measures a warm refinement. A cold worker may legitimately
    // miss the display deadline and leave Mozc's order untouched.
    std::thread::sleep(Duration::from_secs(1));
    let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    let mut client = Client::connect(DeadlineStream::new(stream, KEY_TIMEOUT).unwrap()).unwrap();
    let mut editor = Editor::default();
    let context = Context {
        app: "com.apple.TextEdit".into(),
        privacy: Privacy::Normal as i32,
        surrounding_available: true,
        preceding_text: "明日は市民ホールで有名な作家の".into(),
    };
    for ch in "kouenwokiku".chars() {
        assert!(client.key(
            Key {
                text: ch.into(),
                ..Default::default()
            },
            context.clone(),
            false,
            &mut editor
        ));
    }
    assert!(client.key(
        Key {
            special: SpecialKey::Space as i32,
            ..Default::default()
        },
        context.clone(),
        false,
        &mut editor
    ));
    assert!(
        editor.page.refinement_pending,
        "a normal allowlisted composition must queue inference"
    );
    let before = editor.surface.clone();
    let edits = editor.edits;
    let mut page = editor.page.clone();
    let original_revision = page.candidate_revision;
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(60));
        if let Some(update) = client.poll_candidates(&page).unwrap() {
            page = update;
        }
        if !page.refinement_pending {
            break;
        }
    }
    assert!(
        !page.refinement_pending,
        "inference must finish within the frontend polling window"
    );
    assert!(
        page.candidate_revision > original_revision,
        "the real model must deliver a reordered list"
    );
    assert_eq!(
        editor.edits, edits,
        "polling must not call the document adapter"
    );
    assert!(client.is_connected());
    assert!(client.key(
        Key {
            special: SpecialKey::Enter as i32,
            ..Default::default()
        },
        context,
        false,
        &mut editor
    ));
    assert!(
        editor.committed == before,
        "Enter must keep the original selected surface"
    );
    let removed = manage(&socket, Action::LlmRemove, serde_json::Value::Null);
    assert_eq!(removed["result"], "removed");
    let settings = manage(&socket, Action::GetSettings, serde_json::Value::Null);
    assert_eq!(settings["llm"], false);
}
