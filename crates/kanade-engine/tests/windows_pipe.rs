// SPDX-License-Identifier: MIT
//! The engine process on Windows: its named pipe, its worker and its own
//! survival. Every test starts its own engine on a private endpoint with a
//! temporary data directory, so an installed engine is never reached.
#![cfg(windows)]
mod windows_support;

use kanade_ipc::{
    frontend::{Client, Document},
    windows::Pipe,
    *,
};
use std::{
    io::Write,
    sync::{Arc, Barrier},
    time::{Duration, Instant},
};
use windows_support::{Engine, Frozen, children, wait_until};

const WORKER: &str = "kanade-mozc-worker.exe";

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
        self.preedit = reply.preedit.as_ref().map(surface).unwrap_or_default();
        true
    }
    fn finish_reading(&mut self, reading: &str) -> bool {
        self.committed.push_str(reading);
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
fn letters(client: &mut Client<Pipe>, editor: &mut Editor, text: &str) -> bool {
    text.chars().all(|letter| {
        client.key(
            Key {
                text: letter.into(),
                ..Default::default()
            },
            context(),
            false,
            editor,
        )
    })
}
fn press(client: &mut Client<Pipe>, editor: &mut Editor, key: SpecialKey) -> bool {
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
/// Types a word, converts it and commits the first candidate.
fn convert(client: &mut Client<Pipe>, editor: &mut Editor) -> bool {
    letters(client, editor, "nihon")
        && press(client, editor, SpecialKey::Space)
        && press(client, editor, SpecialKey::Enter)
}
/// A frontend that just started: connects and converts one word.
fn fresh_frontend_converts(engine: &Engine) -> bool {
    let Ok(mut client) = engine.client() else {
        return false;
    };
    let mut editor = Editor::default();
    convert(&mut client, &mut editor) && editor.committed == "日本"
}
fn hello() -> Vec<u8> {
    let mut frame = Vec::new();
    send(
        &mut frame,
        &Request {
            body: Some(request::Body::Hello(Hello {
                min_version: OLDEST_VERSION,
                max_version: CURRENT_VERSION,
            })),
        },
    )
    .unwrap();
    frame
}

#[test]
#[ignore = "requires the locked worker and a debug engine; cargo xtask build-worker"]
fn menu_status_observes_the_worker_without_changing_composition() {
    let engine = Engine::start("menu-status");
    let mut client = engine.client().unwrap();
    let mut editor = Editor::default();
    assert!(letters(&mut client, &mut editor, "ni"));
    let status = || RuntimeStatus::query(&mut engine.pipe().unwrap()).unwrap();
    assert_eq!(
        status().menu_labels(),
        ["変換エンジンの動作：動作中", "AI：オフ"]
    );
    assert!(letters(&mut client, &mut editor, "hon"));
    assert!(press(&mut client, &mut editor, SpecialKey::Space));
    assert!(press(&mut client, &mut editor, SpecialKey::Enter));
    assert_eq!(editor.committed, "日本");
    let [worker] = children(engine.id(), WORKER)[..] else {
        panic!("expected one worker")
    };
    windows_support::terminate(worker);
    assert_eq!(status().engine_running, Some(false));
}

#[test]
#[ignore = "requires the locked worker and a debug engine; cargo xtask build-worker"]
fn a_peer_that_connects_and_leaves_does_not_stop_the_listener() {
    let engine = Engine::start("leave");
    assert!(fresh_frontend_converts(&engine));
    // A host can end, or give up on its handshake, at any point after its
    // pipe opened: before the engine accepted it, before it sent anything,
    // in the middle of a frame, or before reading the reply.
    let barrier = Arc::new(Barrier::new(8));
    let peers: Vec<_> = (0..8)
        .map(|peer| {
            let endpoint = engine.endpoint.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let deadline = Instant::now() + Duration::from_secs(20);
                let mut opened = 0;
                while opened < 40 && Instant::now() < deadline {
                    let Ok(mut pipe) = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&endpoint)
                    else {
                        // Every instance is taken for a moment.
                        std::thread::yield_now();
                        continue;
                    };
                    opened += 1;
                    match (peer + opened) % 4 {
                        0 => {}
                        1 => drop(pipe.write_all(&[9, 0])),
                        2 => drop(pipe.write_all(&[200, 0, 0, 0, 1, 2, 3])),
                        _ => drop(pipe.write_all(&hello())),
                    }
                }
                opened
            })
        })
        .collect();
    let opened: usize = peers.into_iter().map(|peer| peer.join().unwrap()).sum();
    // The engine still accepts frontends, and it is the same process.
    assert!(
        wait_until(Duration::from_secs(10), || fresh_frontend_converts(&engine)),
        "no frontend can connect after {opened} peers left; events: {:?}",
        engine.events()
    );
    assert_eq!(opened, 320, "the pipe stopped being offered");
    assert!(engine.running());
    assert!(
        !engine
            .events()
            .iter()
            .any(|event| event.starts_with("fatal-"))
    );
    // Nothing they left behind counts against the connection limit.
    let full: Vec<_> = (0..32)
        .map(|_| {
            let mut found = None;
            wait_until(Duration::from_secs(10), || {
                found = engine.client().ok();
                found.is_some()
            });
            found.expect("a connection slot stayed taken")
        })
        .collect();
    drop(full);
}

#[test]
#[ignore = "requires the locked worker and a debug engine; cargo xtask build-worker"]
fn many_frontends_convert_at_once_without_losing_a_connection() {
    let engine = Engine::start("many");
    const FRONTENDS: usize = 16;
    const ROUNDS: usize = 25;
    let barrier = Arc::new(Barrier::new(FRONTENDS));
    let frontends: Vec<_> = (0..FRONTENDS)
        .map(|_| {
            let mut client = engine.client().unwrap();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let mut editor = Editor::default();
                for round in 0..ROUNDS {
                    assert!(
                        convert(&mut client, &mut editor),
                        "round {round} was not consumed"
                    );
                    assert!(client.is_connected(), "round {round} lost its connection");
                    assert!(!client.is_held(), "round {round} left a held composition");
                }
                editor.committed
            })
        })
        .collect();
    for frontend in frontends {
        assert_eq!(frontend.join().unwrap(), "日本".repeat(ROUNDS));
    }
    assert!(engine.running());
    let events = engine.events();
    assert!(
        !events
            .iter()
            .any(|event| event.starts_with("worker-") || event.starts_with("fatal-")),
        "{events:?}"
    );
}

#[test]
#[ignore = "requires the locked worker and a debug engine; cargo xtask build-worker"]
fn a_frontend_that_leaves_during_a_request_frees_its_session() {
    let engine = Engine::start("midway");
    for round in 0..96 {
        let mut pipe = engine.pipe().unwrap();
        send(
            &mut pipe,
            &Request {
                body: Some(request::Body::Hello(Hello {
                    min_version: OLDEST_VERSION,
                    max_version: CURRENT_VERSION,
                })),
            },
        )
        .unwrap();
        let welcome: Response = receive(&mut pipe).unwrap();
        let Some(response::Body::Welcome(welcome)) = welcome.body else {
            panic!("expected a welcome");
        };
        let key = Request {
            body: Some(request::Body::Key(KeyRequest {
                revision: Some(Revision {
                    connection: welcome.connection,
                    context: 1,
                    composition: 0,
                }),
                event_seq: 1,
                key: Some(Key {
                    text: "k".into(),
                    ..Default::default()
                }),
                context: Some(context()),
                passed_through: false,
            })),
        };
        match round % 3 {
            // Leaves without reading the reply to a whole request.
            0 => send(&mut pipe, &key).unwrap(),
            // Leaves in the middle of a frame.
            1 => {
                let mut frame = Vec::new();
                send(&mut frame, &key).unwrap();
                pipe.write_all(&frame[..frame.len() / 2]).unwrap();
            }
            // Leaves while a composition is displayed.
            _ => {
                send(&mut pipe, &key).unwrap();
                let _: Response = receive(&mut pipe).unwrap();
            }
        }
    }
    // More frontends left than the engine admits at once. If one of them had
    // kept its session or its slot, these could not all connect.
    let full: Vec<_> = (0..32)
        .map(|_| {
            let mut found = None;
            wait_until(Duration::from_secs(10), || {
                found = engine.client().ok();
                found.is_some()
            });
            found.expect("a session stayed taken")
        })
        .collect();
    drop(full);
    assert!(wait_until(Duration::from_secs(10), || {
        fresh_frontend_converts(&engine)
    }));
    assert!(engine.running());
}

#[test]
#[ignore = "requires the locked worker and a debug engine; cargo xtask build-worker"]
fn a_full_engine_refuses_one_more_frontend_and_says_so() {
    let engine = Engine::start("full");
    let mut frontends: Vec<_> = (0..32).map(|_| engine.client().unwrap()).collect();
    assert!(engine.client().is_err());
    assert!(wait_until(Duration::from_secs(5), || {
        engine
            .events()
            .iter()
            .any(|event| event.starts_with("connection-refused-at-capacity"))
    }));
    // The frontends that were connected are not disturbed.
    let mut editor = Editor::default();
    assert!(convert(&mut frontends[0], &mut editor));
    assert_eq!(editor.committed, "日本");
    frontends.pop();
    assert!(wait_until(Duration::from_secs(10), || {
        fresh_frontend_converts(&engine)
    }));
}

#[test]
#[ignore = "requires the locked worker and a debug engine; cargo xtask build-worker"]
fn a_worker_that_stops_answering_is_replaced_and_the_engine_stays() {
    let engine = Engine::start("silent");
    let mut client = engine.client().unwrap();
    let mut editor = Editor::default();
    assert!(letters(&mut client, &mut editor, "ka"));
    let [worker] = children(engine.id(), WORKER)[..] else {
        panic!("expected one worker");
    };
    let frozen = Frozen::suspend(&[worker]);
    // The key that meets the silent worker is used up; the composition stays.
    assert!(letters(&mut client, &mut editor, "n"));
    assert!(client.is_held());
    assert_eq!(editor.committed, "");
    assert!(wait_until(Duration::from_secs(20), || {
        engine
            .events()
            .iter()
            .any(|event| event.starts_with("worker-restarted"))
    }));
    drop(frozen);
    assert!(
        engine
            .events()
            .iter()
            .any(|event| event.starts_with("worker-call-timed-out"))
    );
    assert_ne!(children(engine.id(), WORKER), [worker]);
    // The frontend opens a new connection and keeps the reading it showed.
    // The key that failed is not typed again by anyone but the user.
    assert!(client.needs_transport());
    client.reattach(engine.pipe().unwrap()).unwrap();
    assert!(letters(&mut client, &mut editor, "na"));
    assert!(!client.is_held());
    assert!(press(&mut client, &mut editor, SpecialKey::Enter));
    assert_eq!(editor.committed, "かな");
    assert!(engine.running());
}

#[test]
#[ignore = "requires the locked worker and a debug engine; cargo xtask build-worker"]
fn a_worker_that_ends_is_replaced_and_the_engine_stays() {
    let engine = Engine::start("ended");
    for _ in 0..5 {
        assert!(fresh_frontend_converts(&engine));
        let [worker] = children(engine.id(), WORKER)[..] else {
            panic!("expected one worker");
        };
        windows_support::terminate(worker);
        // The next frontend meets the missing worker, which is then replaced.
        assert!(
            wait_until(Duration::from_secs(20), || fresh_frontend_converts(&engine)),
            "{:?}",
            engine.events()
        );
        assert!(engine.running());
    }
    let events = engine.events();
    assert!(
        events
            .iter()
            .any(|event| event.starts_with("worker-restarted"))
    );
    assert!(!events.iter().any(|event| event.starts_with("fatal-")));
}

#[test]
#[ignore = "requires the locked worker and a debug engine; cargo xtask build-worker"]
fn typing_continues_after_the_engine_and_its_worker_were_frozen() {
    // A machine that sleeps stops every process and wakes with every pending
    // time limit already past. Freeze both processes for longer than any limit.
    let engine = Engine::start("frozen");
    let mut client = engine.client().unwrap();
    let mut editor = Editor::default();
    for round in 0..3 {
        assert!(letters(&mut client, &mut editor, "ka"));
        let mut processes = children(engine.id(), WORKER);
        processes.push(engine.id());
        let frozen = Frozen::suspend(&processes);
        std::thread::sleep(KEY_TIMEOUT + Duration::from_millis(500));
        drop(frozen);
        // Nothing was in flight, so the same connection and worker continue.
        assert!(letters(&mut client, &mut editor, "na"), "round {round}");
        assert!(!client.is_held(), "round {round}");
        assert!(press(&mut client, &mut editor, SpecialKey::Enter));
        assert_eq!(editor.committed, "かな".repeat(round + 1));
    }
    // A key that was waiting for its reply while everything was frozen is
    // used up; the frontend holds the reading and continues on a new session.
    assert!(letters(&mut client, &mut editor, "ka"));
    let mut processes = children(engine.id(), WORKER);
    processes.push(engine.id());
    let frozen = Frozen::suspend(&processes);
    let started = Instant::now();
    assert!(letters(&mut client, &mut editor, "n"));
    assert!(started.elapsed() >= KEY_TIMEOUT - Duration::from_millis(100));
    assert!(client.is_held());
    drop(frozen);
    assert!(wait_until(Duration::from_secs(20), || {
        client.needs_transport()
            && engine
                .pipe()
                .is_ok_and(|pipe| client.reattach(pipe).is_ok())
    }));
    assert!(letters(&mut client, &mut editor, "na"));
    assert!(!client.is_held());
    assert!(press(&mut client, &mut editor, SpecialKey::Enter));
    assert_eq!(editor.committed, "かな".repeat(4));
    assert!(engine.running());
    assert!(
        !engine
            .events()
            .iter()
            .any(|event| event.starts_with("fatal-"))
    );
}

#[test]
#[ignore = "requires the locked worker and a debug engine; cargo xtask build-worker"]
fn a_frontend_notices_a_replaced_engine_before_its_next_key() {
    let mut engine = Engine::start("replaced");
    let mut client = engine.client().unwrap();
    let mut editor = Editor::default();
    assert!(convert(&mut client, &mut editor));
    assert!(!client.needs_transport());
    engine.restart();
    // Without this the first key after a restart would fail its exchange and
    // reach the application as a plain letter.
    assert!(client.needs_transport());
    client.reattach(engine.pipe().unwrap()).unwrap();
    assert!(convert(&mut client, &mut editor));
    assert_eq!(editor.committed, "日本日本");
}

#[test]
#[ignore = "requires the locked worker and a debug engine; cargo xtask build-worker"]
fn an_engine_started_at_sign_in_has_no_console_window_to_close() {
    let engine = Engine::start_as_at_sign_in("console");
    assert!(fresh_frontend_converts(&engine));
    let [worker] = children(engine.id(), WORKER)[..] else {
        panic!("expected one worker");
    };
    // A console window can be closed, and closing it ends every process
    // attached to it without a record. The engine gets no console at all.
    // Its worker is a console program and gets one without a window, so that
    // a helper it starts does not open a window of its own.
    assert!(children(engine.id(), "conhost.exe").is_empty());
    let mut tree = vec![engine.id(), worker];
    tree.extend(children(worker, "conhost.exe"));
    assert_eq!(windows_support::console_windows(&tree), 0);
}

#[cfg(feature = "fault-injection")]
#[test]
#[ignore = "requires the locked worker and a debug engine; cargo xtask build-worker"]
fn a_panic_while_handling_a_request_is_recorded_and_the_engine_stays() {
    use kanade_engine::server::fault;
    let engine = Engine::start("panic");
    let mut bystander = engine.client().unwrap();
    let mut bystander_editor = Editor::default();
    assert!(letters(&mut bystander, &mut bystander_editor, "ka"));
    for round in 1..=3 {
        let mut client = engine.client().unwrap();
        let mut editor = Editor::default();
        // The connection learns of the failure at once instead of waiting for
        // its limit, so the frontend passes the key on without a pause.
        let started = Instant::now();
        let key = Key {
            text: fault::PANIC_KEY.into(),
            ..Default::default()
        };
        assert!(!client.key(key, context(), false, &mut editor));
        assert!(started.elapsed() < REQUEST_TIMEOUT, "round {round}");
        assert!(
            wait_until(Duration::from_secs(20), || fresh_frontend_converts(&engine)),
            "round {round}: {:?}",
            engine.events()
        );
    }
    assert!(engine.running());
    let events = engine.events();
    let panics: Vec<_> = events
        .iter()
        .filter(|event| event.starts_with("panicked "))
        .collect();
    assert_eq!(panics.len(), 3, "{events:?}");
    for panic in panics {
        // Which thread and which source line, never the message.
        assert!(panic.contains("thread=main"), "{panic}");
        assert!(panic.contains("at=kanade-engine/src/server.rs:"), "{panic}");
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("recovered-from-panic"))
            .count(),
        3
    );
    assert!(!engine.log().contains(fault::PANIC_MESSAGE));
    // The composition another frontend was showing is held, not committed.
    assert!(letters(&mut bystander, &mut bystander_editor, "n"));
    assert!(bystander.is_held());
    assert_eq!(bystander_editor.committed, "");
}

/// Everything at once, for `KANADE_SOAK_SECONDS` (twenty by default):
/// frontends that convert without a pause, peers that connect and leave, a
/// worker that is ended every other second and a freeze of both processes.
/// Whatever each frontend meets, the engine process must still be the same
/// one at the end, with no fatal record and no panic.
#[test]
#[ignore = "requires the locked worker and a debug engine; cargo xtask build-worker"]
fn mixed_load_for_a_while_never_ends_the_engine() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let seconds = std::env::var("KANADE_SOAK_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20);
    let engine = Arc::new(Engine::start("soak"));
    let process = engine.id();
    let stop = Arc::new(AtomicBool::new(false));
    let converted = Arc::new(AtomicUsize::new(0));
    let interrupted = Arc::new(AtomicUsize::new(0));
    let frontends: Vec<_> = (0..8)
        .map(|_| {
            let (engine, stop) = (engine.clone(), stop.clone());
            let (converted, interrupted) = (converted.clone(), interrupted.clone());
            std::thread::spawn(move || {
                let mut client = None;
                let mut editor = Editor::default();
                let mut own = 0;
                while !stop.load(Ordering::Acquire) {
                    let Some(connected) = client.as_mut() else {
                        client = engine.client().ok();
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    };
                    if connected.needs_transport()
                        && !engine
                            .pipe()
                            .is_ok_and(|pipe| connected.reattach(pipe).is_ok())
                    {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    let before = editor.committed.len();
                    if convert(connected, &mut editor) && &editor.committed[before..] == "日本" {
                        own += 1;
                        converted.fetch_add(1, Ordering::Relaxed);
                    } else {
                        // The worker was replaced or everything was frozen.
                        // End what is displayed, as leaving the field does.
                        interrupted.fetch_add(1, Ordering::Relaxed);
                        assert!(!connected.finish(&mut editor));
                    }
                }
                own
            })
        })
        .collect();
    let leavers: Vec<_> = (0..2)
        .map(|_| {
            let (endpoint, stop) = (engine.endpoint.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    if let Ok(mut pipe) = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&endpoint)
                    {
                        let _ = pipe.write_all(&hello()[..3]);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut round = 0;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(2));
        round += 1;
        let workers = children(process, WORKER);
        if round % 3 == 0 {
            let mut processes = workers;
            processes.push(process);
            let frozen = Frozen::suspend(&processes);
            std::thread::sleep(KEY_TIMEOUT + Duration::from_millis(300));
            drop(frozen);
        } else if let Some(&worker) = workers.first() {
            windows_support::terminate(worker);
        }
    }
    stop.store(true, Ordering::Release);
    let each: Vec<usize> = frontends
        .into_iter()
        .map(|frontend| frontend.join().unwrap())
        .collect();
    for leaver in leavers {
        leaver.join().unwrap();
    }
    let events = engine.events();
    println!(
        "{seconds} s: {} conversions, {} interrupted, per frontend {each:?}, {} worker restarts",
        converted.load(Ordering::Relaxed),
        interrupted.load(Ordering::Relaxed),
        events
            .iter()
            .filter(|event| event.starts_with("worker-restarted"))
            .count()
    );
    assert!(engine.running());
    assert_eq!(engine.id(), process);
    assert!(
        !events
            .iter()
            .any(|event| event.starts_with("fatal-") || event.starts_with("panicked")),
        "{events:?}"
    );
    // Nobody was shut out: every frontend converted between the disturbances.
    assert!(each.iter().all(|&own| own > 0), "{each:?}");
    assert!(wait_until(Duration::from_secs(20), || {
        fresh_frontend_converts(&engine)
    }));
}

#[test]
#[ignore = "requires the locked worker and a debug engine; cargo xtask build-worker"]
fn files_that_cannot_be_read_leave_conversion_working_and_unchanged() {
    // Written by a newer version, and cut off in the middle of a record.
    let settings = "learning = true\nfrom_a_newer_version = 1\n";
    let learning = "{\"records\":[{\"id\":";
    let mut engine = Engine::start_with_files(
        "unreadable",
        &[
            ("kanade/settings.toml", settings),
            ("kanade/learning.json", learning),
        ],
    );
    assert!(fresh_frontend_converts(&engine));
    let events = engine.events();
    for expected in ["settings-unreadable", "learning-store-unreadable"] {
        assert!(events.iter().any(|event| event == expected), "{events:?}");
    }
    assert!(
        !events.iter().any(|event| event.starts_with("fatal-")),
        "{events:?}"
    );
    engine.stop();
    let read = |path: &str| std::fs::read_to_string(engine.data().join(path)).unwrap();
    assert_eq!(read("kanade/settings.toml"), settings);
    assert_eq!(read("kanade/learning.json"), learning);
}

#[test]
fn the_engine_is_built_without_a_console() {
    // Subsystem 2 is the windowed one: the system gives such a process no
    // console window, however it is started.
    let image = std::fs::read(env!("CARGO_BIN_EXE_kanade-engine")).unwrap();
    let header = u32::from_le_bytes(image[0x3c..0x40].try_into().unwrap()) as usize;
    assert_eq!(&image[header..header + 4], b"PE\0\0");
    let subsystem = u16::from_le_bytes(image[header + 92..header + 94].try_into().unwrap());
    assert_eq!(subsystem, 2);
}
