// SPDX-License-Identifier: MIT
//! Explore candidate boundaries without borrowing the live input process or its profile.
use super::{Proposal, alternatives};
use kanade_ipc::{Preedit, Privacy, RefinementOutcome, WORKER_KEY_TIMEOUT, diagnostics};
use kanade_llm::profile::Preview;
use kanade_mozc::{ConversionEngine, Mozc};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    task::Poll,
    time::{Duration, Instant},
};

type Answer = Result<Option<Vec<Proposal>>, RefinementOutcome>;

struct Input {
    preedit: Preedit,
    context: String,
    years: bool,
    policy: Preview,
}

struct Request {
    input: Weak<Input>,
    expires: Instant,
    reply: SyncSender<Answer>,
}

pub(super) struct Ticket {
    // Dropping a pending preview cancels work that hasn't started.
    _input: Arc<Input>,
    result: Receiver<Answer>,
}

impl Ticket {
    pub(super) fn poll(&self) -> Poll<Answer> {
        match self.result.try_recv() {
            Ok(result) => Poll::Ready(result),
            Err(TryRecvError::Empty) => Poll::Pending,
            Err(TryRecvError::Disconnected) => Poll::Ready(Err(RefinementOutcome::Unavailable)),
        }
    }
}

pub(crate) struct Worker {
    binary: PathBuf,
    jobs: Option<SyncSender<Request>>,
}

impl Worker {
    pub(crate) fn new(binary: &Path) -> Self {
        Self {
            binary: binary.to_owned(),
            jobs: None,
        }
    }

    pub(crate) fn stop(&mut self) {
        self.jobs.take();
    }

    pub(super) fn submit(
        &mut self,
        preedit: &Preedit,
        context: &str,
        years: bool,
        policy: &Preview,
        expires: Instant,
    ) -> Option<Ticket> {
        if self.jobs.is_none() {
            let (tx, rx) = mpsc::sync_channel(1);
            let binary = self.binary.clone();
            std::thread::Builder::new()
                .name(diagnostics::Thread::SENTENCE_SEARCH.into())
                .spawn(move || run(&binary, rx))
                .ok()?;
            self.jobs = Some(tx);
        }
        let input = Arc::new(Input {
            preedit: preedit.clone(),
            context: context.to_owned(),
            years,
            policy: *policy,
        });
        let (reply, result) = mpsc::sync_channel(1);
        let request = Request {
            input: Arc::downgrade(&input),
            expires,
            reply,
        };
        match self.jobs.as_ref()?.try_send(request) {
            Ok(()) => Some(Ticket {
                _input: input,
                result,
            }),
            Err(TrySendError::Full(_)) => None,
            Err(TrySendError::Disconnected(_)) => {
                self.stop();
                None
            }
        }
    }
}

fn open(binary: &Path) -> kanade_mozc::Result<(Mozc, tempfile::TempDir)> {
    let profile = tempfile::tempdir()?;
    let mozc = Mozc::start_probe(binary, profile.path())?;
    Ok((mozc, profile))
}

fn run(binary: &Path, jobs: Receiver<Request>) {
    let mut process = None;
    let mut retry_at = Instant::now();
    for request in jobs {
        if request.input.strong_count() == 0 {
            continue;
        }
        if Instant::now() >= request.expires {
            let _ = request.reply.send(Err(RefinementOutcome::TimedOut));
            continue;
        }
        if process.is_none() && Instant::now() >= retry_at {
            process = open(binary).ok();
            if process.is_none() {
                diagnostics::record(diagnostics::Event::ProbeCallFailed);
                retry_at = Instant::now() + Duration::from_secs(1);
            }
        }
        let Some((backend, _profile)) = &mut process else {
            let _ = request.reply.send(Err(RefinementOutcome::Unavailable));
            continue;
        };
        let Some(input) = request.input.upgrade() else {
            continue;
        };
        let deadline = request.expires.min(Instant::now() + WORKER_KEY_TIMEOUT);
        let result = if Instant::now() >= deadline {
            Err(RefinementOutcome::TimedOut)
        } else {
            backend.set_deadline(Some(deadline));
            let proposal = backend.set_privacy(Privacy::Normal).map(|()| {
                alternatives(
                    backend,
                    &input.preedit,
                    &input.context,
                    input.years,
                    &input.policy,
                )
            });
            if Instant::now() >= deadline {
                Err(RefinementOutcome::TimedOut)
            } else if !backend.healthy() {
                Err(RefinementOutcome::Unavailable)
            } else {
                proposal.map_err(|_| RefinementOutcome::Unavailable)
            }
        };
        if !backend.healthy() {
            process = None;
            retry_at = Instant::now() + Duration::from_secs(1);
        }
        let _ = request.reply.send(result);
    }
}
