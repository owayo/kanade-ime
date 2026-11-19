// SPDX-License-Identifier: MIT
//! Parent side of the worker process: private pipes, hard deadlines and kill.
use crate::{
    Job,
    frame::{self, Fault, Loaded, Reply, Request},
    limits,
    profile::{self, Model, Profile},
};
use std::{
    io::BufReader,
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    time::Duration,
};

/// Fixed failure reasons; none carries request content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The executable is missing or cannot start.
    Spawn,
    /// No answer before the hard deadline. The process has been killed.
    Timeout,
    /// The process exited or closed its pipe.
    Closed,
    /// A frame was malformed, out of bounds or carried a non-finite score.
    Malformed,
    /// A well-formed answer that does not belong to the request.
    Protocol,
    /// The worker runs a different protocol or runtime than this build pins.
    Runtime,
    /// The worker loaded the file, but what it reports is not the registered
    /// model and profile. The process has been killed.
    Identity,
    /// The worker refused this request and remains usable.
    Refused(Fault),
}
impl Failure {
    /// Whether the process can serve another request.
    pub fn recoverable(self) -> bool {
        matches!(
            self,
            Self::Refused(
                Fault::TooLong
                    | Fault::Deadline
                    | Fault::Tokenizer
                    | Fault::Decode
                    | Fault::Reserved
            )
        )
    }
}

pub struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    replies: Receiver<Option<Reply>>,
    next: u64,
    dead: bool,
    /// Profile of the model this process loaded and was verified against.
    profile: Option<&'static Profile>,
}

/// Compares what the worker loaded and applied with the registered model.
pub fn identity_matches(loaded: &Loaded, model: &Model) -> bool {
    let profile = model.profile;
    profile::hex(&loaded.model_sha256) == model.sha256
        && loaded.model_size == model.size
        && profile::hex(&loaded.vocab_sha256) == profile.vocab_sha256
        && loaded.vocab_size == profile.vocab_size
        && loaded.profile_sha256 == profile.digest()
        && loaded.start == profile.start
        && loaded.end == profile.end
        && loaded.context_tokens == limits::CONTEXT_TOKENS
        && (1..=limits::THREADS).contains(&loaded.threads)
}

impl Worker {
    pub const START_TIMEOUT: Duration = Duration::from_secs(3);
    pub const LOAD_TIMEOUT: Duration = Duration::from_secs(20);

    /// Starts the process and checks that it is the pinned runtime.
    pub fn spawn(program: &Path) -> Result<Self, Failure> {
        let mut command = Command::new(program);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_clear();
        #[cfg(windows)]
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        let mut child = command.spawn().map_err(|_| Failure::Spawn)?;
        let input = child.stdin.take();
        let Some(output) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Failure::Spawn);
        };
        let (sender, replies) = mpsc::sync_channel(2);
        let reader = std::thread::Builder::new()
            .name("kanade-llm-reader".into())
            .spawn(move || {
                let mut output = BufReader::new(output);
                while let Ok(body) = frame::read(&mut output) {
                    let reply = Reply::decode(&body);
                    let stop = reply.is_none();
                    if sender.send(reply).is_err() || stop {
                        break;
                    }
                }
            });
        let mut worker = Self {
            child,
            input,
            replies,
            next: 0,
            dead: false,
            profile: None,
        };
        if reader.is_err() {
            worker.kill();
            return Err(Failure::Spawn);
        }
        match worker.receive(Self::START_TIMEOUT)? {
            Reply::Hello {
                protocol,
                runtime,
                accelerators: 0,
            } if protocol == frame::PROTOCOL && runtime == profile::RUNTIME => Ok(worker),
            Reply::Hello { .. } => {
                worker.kill();
                Err(Failure::Runtime)
            }
            _ => Err(worker.fail(Failure::Protocol)),
        }
    }
    /// Loads `path` as the registered `model`. The worker selects the model's
    /// profile from its own registry; the answer is accepted only when file,
    /// vocabulary and applied profile are the ones this build registers.
    pub fn load(
        &mut self,
        path: &Path,
        model: &'static Model,
        threads: u8,
    ) -> Result<Loaded, Failure> {
        let path = path
            .to_str()
            .ok_or(Failure::Refused(Fault::ModelUnreadable))?;
        self.send(&Request::Load {
            path: path.to_owned(),
            threads,
            model: model.id.to_owned(),
        })?;
        match self.receive(Self::LOAD_TIMEOUT)? {
            Reply::Loaded(loaded) if identity_matches(&loaded, model) => {
                self.profile = Some(model.profile);
                Ok(loaded)
            }
            Reply::Loaded(_) => Err(self.fail(Failure::Identity)),
            Reply::Error { fault, .. } => Err(self.fail(Failure::Refused(fault))),
            _ => Err(self.fail(Failure::Protocol)),
        }
    }
    /// One score per candidate, or a failure. Never waits beyond `timeout`.
    ///
    /// A job with a marker or reserved character of the loaded profile is
    /// refused here, before any of its text is written to the pipe.
    pub fn score(
        &mut self,
        job: &Job,
        budget_ms: u32,
        timeout: Duration,
    ) -> Result<Vec<Option<f32>>, Failure> {
        self.score_conversion(job, budget_ms, timeout, false)
    }
    /// Sentence comparisons can include the character probabilities in the registered GGUF.
    pub fn score_conversion(
        &mut self,
        job: &Job,
        budget_ms: u32,
        timeout: Duration,
        sentence: bool,
    ) -> Result<Vec<Option<f32>>, Failure> {
        if !job.valid() {
            return Err(Failure::Refused(Fault::Malformed));
        }
        if self
            .profile
            .is_some_and(|profile| !profile.prompt.admits(job))
        {
            return Err(Failure::Refused(Fault::Reserved));
        }
        self.score_unchecked(job, budget_ms, timeout, sentence)
    }
    /// Sends the job without the profile check, so that qualification can
    /// observe the worker's own refusal.
    pub(crate) fn score_unchecked(
        &mut self,
        job: &Job,
        budget_ms: u32,
        timeout: Duration,
        sentence: bool,
    ) -> Result<Vec<Option<f32>>, Failure> {
        if !job.valid() {
            return Err(Failure::Refused(Fault::Malformed));
        }
        let id = self.id();
        self.send(&Request::Score {
            sentence,
            id,
            budget_ms,
            job: job.clone(),
        })?;
        match self.receive(timeout)? {
            Reply::Scores {
                id: answered,
                scores,
                ..
            } if answered == id && scores.len() == job.candidates.len() => Ok(scores),
            Reply::Error {
                id: answered,
                fault,
            } if answered == id => {
                let failure = Failure::Refused(fault);
                if failure.recoverable() {
                    Err(failure)
                } else {
                    Err(self.fail(failure))
                }
            }
            _ => Err(self.fail(Failure::Protocol)),
        }
    }
    pub fn tokenize(
        &mut self,
        texts: &[String],
        timeout: Duration,
    ) -> Result<Vec<Option<Vec<u32>>>, Failure> {
        let id = self.id();
        self.send(&Request::Tokenize {
            id,
            texts: texts.to_vec(),
        })?;
        match self.receive(timeout)? {
            Reply::Tokens { id: answered, ids } if answered == id && ids.len() == texts.len() => {
                Ok(ids)
            }
            Reply::Error { fault, .. } => Err(self.fail(Failure::Refused(fault))),
            _ => Err(self.fail(Failure::Protocol)),
        }
    }
    pub fn alive(&self) -> bool {
        !self.dead
    }
    /// Stops the process now. Safe to call repeatedly.
    pub fn kill(&mut self) {
        self.dead = true;
        self.input = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
    fn fail(&mut self, failure: Failure) -> Failure {
        self.kill();
        failure
    }
    fn send(&mut self, request: &Request) -> Result<(), Failure> {
        if self.dead {
            return Err(Failure::Closed);
        }
        let sent = match self.input.as_mut() {
            Some(input) => frame::write(input, &request.encode()).is_ok(),
            None => false,
        };
        if sent {
            Ok(())
        } else {
            Err(self.fail(Failure::Closed))
        }
    }
    fn receive(&mut self, timeout: Duration) -> Result<Reply, Failure> {
        match self.replies.recv_timeout(timeout) {
            Ok(Some(reply)) => Ok(reply),
            Ok(None) => Err(self.fail(Failure::Malformed)),
            Err(RecvTimeoutError::Timeout) => Err(self.fail(Failure::Timeout)),
            Err(RecvTimeoutError::Disconnected) => Err(self.fail(Failure::Closed)),
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(mut input) = self.input.take() {
            let _ = frame::write(&mut input, &Request::Shutdown.encode());
        }
        // Closing the pipe ends a healthy worker; do not wait on a stuck one.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{CATALOG, fixture};

    fn loaded(model: &Model) -> Loaded {
        let bytes = |hex: &str| -> [u8; 32] {
            std::array::from_fn(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
        };
        Loaded {
            model_sha256: bytes(model.sha256),
            model_size: model.size,
            vocab_sha256: bytes(model.profile.vocab_sha256),
            vocab_size: model.profile.vocab_size,
            profile_sha256: model.profile.digest(),
            start: model.profile.start,
            end: model.profile.end,
            context_tokens: limits::CONTEXT_TOKENS,
            threads: limits::THREADS,
        }
    }
    #[test]
    fn a_load_report_is_accepted_only_for_the_selected_model_and_profile() {
        let tsumugi = &CATALOG[0];
        let alt = fixture::alt();
        assert!(identity_matches(&loaded(tsumugi), tsumugi));
        assert!(identity_matches(&loaded(alt), alt));
        // Same file and vocabulary, but the worker applied another profile.
        assert!(!identity_matches(&loaded(alt), tsumugi));
        assert!(!identity_matches(&loaded(tsumugi), alt));
        assert!(!identity_matches(&loaded(tsumugi), fixture::other()));
        type Edit = fn(&mut Loaded);
        let edits: [Edit; 9] = [
            |l| l.model_sha256[0] ^= 1,
            |l| l.model_size += 1,
            |l| l.vocab_sha256[31] ^= 1,
            |l| l.vocab_size += 1,
            |l| l.profile_sha256[5] ^= 1,
            |l| l.start = Some(1),
            |l| l.end = 2,
            |l| l.context_tokens += 1,
            |l| l.threads = limits::THREADS + 1,
        ];
        for edit in edits {
            let mut report = loaded(tsumugi);
            edit(&mut report);
            assert!(!identity_matches(&report, tsumugi));
        }
    }
    #[cfg(unix)]
    #[test]
    fn missing_silent_and_garbage_workers_are_refused() {
        assert_eq!(
            Worker::spawn(Path::new("/nonexistent/kanade-llm-worker")).err(),
            Some(Failure::Spawn)
        );
        // Exits at once without a greeting.
        assert_eq!(
            Worker::spawn(Path::new("/usr/bin/true")).err(),
            Some(Failure::Closed)
        );
        // Prints bytes that are not a bounded frame.
        assert_eq!(
            Worker::spawn(Path::new("/usr/bin/yes")).err(),
            Some(Failure::Closed)
        );
    }
}
