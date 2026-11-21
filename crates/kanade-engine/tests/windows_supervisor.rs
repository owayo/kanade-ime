// SPDX-License-Identifier: MIT
//! The supervisor process on Windows: it starts the engine again whenever it
//! ends and records how it ended. Every test uses a private endpoint and a
//! temporary data directory, so an installed engine is never reached.
#![cfg(windows)]
mod windows_support;

use kanade_ipc::{
    frontend::{Client, Document},
    windows::Pipe,
    *,
};
use std::time::Duration;
use windows_support::{Engine, label, terminate, wait_until};

#[derive(Default)]
struct Editor(String);
impl Document for Editor {
    fn apply(&mut self, reply: &KeyReply) -> bool {
        if let Some(commit) = &reply.commit {
            self.0.push_str(&commit.value);
        }
        true
    }
    fn finish_reading(&mut self, reading: &str) -> bool {
        self.0.push_str(reading);
        true
    }
}
fn convert(client: &mut Client<Pipe>, editor: &mut Editor) -> bool {
    let context = || Context {
        privacy: Privacy::Private as i32,
        ..Default::default()
    };
    let letters = "nihon".chars().map(|letter| Key {
        text: letter.into(),
        ..Default::default()
    });
    let specials = [SpecialKey::Space, SpecialKey::Enter].map(|special| Key {
        special: special as i32,
        ..Default::default()
    });
    letters
        .chain(specials)
        .all(|key| client.key(key, context(), false, editor))
}
fn count(events: &[String], event: &str) -> usize {
    events.iter().filter(|line| *line == event).count()
}

#[test]
fn an_engine_that_cannot_start_is_tried_again_and_each_exit_is_recorded() {
    let label = label("unstartable");
    let missing_worker = |command: &mut std::process::Command| {
        command
            .arg("--supervise")
            .env("KANADE_MOZC_WORKER", r"C:\kanade-test\no-such-worker.exe");
    };
    let supervisor = Engine::spawn_with(&label, missing_worker);
    assert!(
        wait_until(Duration::from_secs(30), || {
            supervisor
                .events()
                .iter()
                .any(|event| event == "engine-restarted count=2")
        }),
        "{:?}",
        supervisor.events()
    );
    let events = supervisor.events();
    // The engine says why it could not start; the supervisor says how it
    // ended and that it started it again. Nothing else is written.
    assert_eq!(
        events[..7],
        [
            "watch-started",
            "fatal-worker-start",
            "engine-exited code=1",
            "engine-restarted count=1",
            "fatal-worker-start",
            "engine-exited code=1",
            "engine-restarted count=2",
        ]
    );
    // A second supervisor for the same endpoint leaves the first in charge.
    let mut second = Engine::spawn_with(&label, missing_worker);
    assert_eq!(second.exit_code(Duration::from_secs(10)), Some(0));
    assert!(second.events().is_empty());
    assert!(supervisor.running());
    assert_eq!(count(&supervisor.events(), "watch-started"), 1);
}

#[test]
fn an_unknown_mode_is_refused() {
    let mut engine = Engine::spawn_with(&label("usage"), |command| {
        command.arg("--watch");
    });
    assert_eq!(engine.exit_code(Duration::from_secs(10)), Some(64));
    assert!(engine.events().is_empty());
}

#[test]
#[ignore = "requires the locked worker and a debug engine; cargo xtask build-worker"]
fn an_engine_that_ends_is_started_again_and_frontends_continue() {
    let mut supervisor = Engine::start_supervised("ended");
    let mut client = supervisor.client().unwrap();
    let mut editor = Editor::default();
    assert!(convert(&mut client, &mut editor));
    for round in 1..=3 {
        let [engine] = supervisor.supervised()[..] else {
            panic!("expected one engine");
        };
        // Ended from outside, as by a closed console window or a task manager.
        terminate(engine);
        supervisor.wait_for_start(round + 1);
        assert_ne!(supervisor.supervised(), [engine]);
        // The frontend notices before its next key and continues on the new
        // engine, so no key reaches the application as a plain letter.
        assert!(client.needs_transport());
        assert!(wait_until(Duration::from_secs(10), || {
            supervisor
                .pipe()
                .is_ok_and(|pipe| client.reattach(pipe).is_ok())
        }));
        assert!(convert(&mut client, &mut editor));
    }
    assert_eq!(editor.0, "日本".repeat(4));
    let events = supervisor.events();
    assert_eq!(count(&events, "watch-started"), 1);
    assert_eq!(count(&events, "engine-started"), 4);
    assert_eq!(count(&events, "engine-exited code=1"), 3);
    for restart in 1..=3 {
        assert_eq!(
            count(&events, &format!("engine-restarted count={restart}")),
            1
        );
    }
    assert!(supervisor.running());
}

#[test]
#[ignore = "requires the locked worker and a debug engine; cargo xtask build-worker"]
fn an_engine_started_by_other_means_is_taken_over_only_after_it_ends() {
    // An engine that an older sign-in entry started directly.
    let other = Engine::start("other");
    let supervisor = Engine::spawn_with(other.label(), |command| {
        command.arg("--supervise");
    });
    assert!(wait_until(Duration::from_secs(10), || {
        supervisor.events() == ["watch-started"]
    }));
    std::thread::sleep(Duration::from_secs(5));
    // No second engine was started against the one that serves.
    assert_eq!(supervisor.events(), ["watch-started"]);
    assert!(supervisor.supervised().is_empty());
    assert!(other.running());
    drop(other);
    assert!(
        wait_until(Duration::from_secs(30), || {
            supervisor
                .events()
                .iter()
                .any(|event| event == "engine-started")
        }),
        "{:?}",
        supervisor.events()
    );
    let mut client = supervisor.client().unwrap();
    let mut editor = Editor::default();
    assert!(convert(&mut client, &mut editor));
    assert_eq!(editor.0, "日本");
    assert_eq!(supervisor.events(), ["watch-started", "engine-started"]);
}
