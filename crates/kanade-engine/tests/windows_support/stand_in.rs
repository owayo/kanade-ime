// SPDX-License-Identifier: MIT
//! Stands in for a frontend in a process of its own, in the tests of this
//! package. Built only with the `fault-injection` feature; no shipped build
//! has it.
//!
//! A test process cannot be every host a text service runs in. With
//! `type <endpoint> <engine>` this program connects to that engine as a
//! frontend does, types a word and exits with what came of it.

/// A command line this program does not know.
const USAGE: i32 = 64;

#[cfg(windows)]
mod frontend {
    use kanade_ipc::{
        Context, Key, KeyReply, Privacy, SpecialKey,
        frontend::{Client, Document},
        windows::Pipe,
    };
    use std::{ffi::OsStr, path::Path};

    /// Connects as a text service does. No connection is reported as the
    /// system's error code, kept apart from every other result.
    fn connect(endpoint: &OsStr, engine: &OsStr) -> Result<Pipe, i32> {
        let endpoint = endpoint.to_str().ok_or(super::USAGE)?;
        Pipe::connect_to(endpoint, Path::new(engine))
            .map_err(|error| 1000 + error.raw_os_error().unwrap_or(0))
    }
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
    /// 0 when a word typed in romaji was committed as kana.
    pub fn type_word(endpoint: &OsStr, engine: &OsStr) -> i32 {
        let pipe = match connect(endpoint, engine) {
            Ok(pipe) => pipe,
            Err(code) => return code,
        };
        let Ok(mut client) = Client::connect(pipe) else {
            return 3;
        };
        let mut editor = Editor(String::new());
        let context = || Context {
            privacy: Privacy::Private as i32,
            ..Default::default()
        };
        let letters = "kana".chars().map(|letter| Key {
            text: letter.into(),
            ..Default::default()
        });
        let enter = Key {
            special: SpecialKey::Enter as i32,
            ..Default::default()
        };
        let consumed = letters
            .chain(Some(enter))
            .all(|key| client.key(key, context(), false, &mut editor));
        if consumed && editor.0 == "かな" {
            0
        } else {
            1
        }
    }
}

fn main() {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let code = match arguments.as_slice() {
        #[cfg(windows)]
        [mode, endpoint, engine] if mode == "type" => frontend::type_word(endpoint, engine),
        _ => USAGE,
    };
    std::process::exit(code);
}
