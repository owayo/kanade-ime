//! Exercise an installed, opted-in Windows IME with an in-memory document.
#![cfg(windows)]
use kanade_ipc::{
    frontend::*,
    windows::{Pipe, Program},
    *,
};
use std::{path::PathBuf, time::Duration};

#[derive(Default)]
struct Editor {
    page: CandidatePage,
    edits: usize,
}

impl Document for Editor {
    fn apply(&mut self, reply: &KeyReply) -> bool {
        assert!(reply.commit.is_none());
        self.page = reply.candidates.clone().unwrap_or_default();
        self.edits += 1;
        true
    }

    fn finish_reading(&mut self, _: &str) -> bool {
        panic!("the installed engine disconnected");
    }
}

#[test]
#[ignore = "requires the installed Windows IME with Tsumugi enabled and normal learning policy"]
fn installed_engine_refines_a_conversion_over_the_native_pipe() {
    let engine = PathBuf::from(std::env::var_os("ProgramW6432").unwrap())
        .join("Kanade")
        .join("kanade-engine.exe");
    let mut client = Client::connect(Pipe::connect(&Program::Win32(engine)).unwrap()).unwrap();
    let mut editor = Editor::default();
    let context = Context {
        app: "kanade.integration.test".into(),
        privacy: Privacy::Normal as i32,
        surrounding_available: true,
        preceding_text: "明日は市民ホールで有名な作家の".into(),
    };
    for letter in "kouenwokiku".chars() {
        let key = Key {
            text: letter.into(),
            ..Default::default()
        };
        assert!(client.key(key, context.clone(), false, &mut editor));
    }
    let space = Key {
        special: SpecialKey::Space as i32,
        ..Default::default()
    };
    assert!(client.key(space, context, false, &mut editor));
    let edits = editor.edits;
    let before = editor.page.clone();
    assert!(before.refinement_pending);
    let mut page = before.clone();
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(25));
        if let Some(refined) = client.poll_candidates(&page).unwrap() {
            page = refined;
        }
        if !page.refinement_pending {
            break;
        }
    }
    assert!(!page.refinement_pending);
    assert!(page.candidate_revision > before.candidate_revision);
    assert!(
        page.items
            .iter()
            .map(|item| &item.value)
            .ne(before.items.iter().map(|item| &item.value))
    );
    assert_eq!(edits, editor.edits);
}
