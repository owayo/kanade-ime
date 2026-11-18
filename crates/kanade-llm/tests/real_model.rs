// SPDX-License-Identifier: MIT
//! Real inference with the pinned runtime and model.
//!
//! Build the worker with `--features llama` and pass the model explicitly:
//!
//! ```sh
//! KANADE_LLM_MODEL=/path/to/tsumugi.gguf \
//!   cargo test -p kanade-llm --features llama --test real_model -- --ignored --nocapture --test-threads 1
//! ```
//!
//! Adding `--features llama,fixture-profiles` also runs the contracts that load
//! the same file under a test-only profile.
//!
//! Output is counts, digests and durations. No corpus text is printed.
#![cfg(feature = "llama")]
use kanade_llm::{
    Job, Manager, Poll, Rerank,
    frame::{self, Fault, Reply, Request},
    katakana, limits,
    manager::{ImportRequest, JobError, JobPhase, Lifecycle, RemoveResult, StartResult, Status},
    process::identity_matches,
    process::{Failure, Worker},
    profile, qualify,
    store::{Reason, Recorded, Store},
};
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const WORKER: &str = env!("CARGO_BIN_EXE_kanade-llm-worker");
/// Never appears in any corpus; searched for in everything the worker emits.
const CANARY: &str = "鍵束七三九一";

fn model_path() -> PathBuf {
    std::env::var_os("KANADE_LLM_MODEL")
        .expect("set KANADE_LLM_MODEL to the pinned model file")
        .into()
}
fn model() -> &'static profile::Model {
    &profile::CATALOG[0]
}
fn job(context: &str) -> Job {
    Job {
        context: context.into(),
        reading: katakana("こうえんをきく"),
        prefix: String::new(),
        suffix: "を聞く".into(),
        candidates: ["公園", "講演", "公演", "後援"].map(String::from).to_vec(),
    }
}

#[test]
#[ignore = "needs KANADE_LLM_MODEL and the worker built with --features llama"]
fn qualification_measures_the_real_runtime_and_model() {
    let mut phases = Vec::new();
    let started = Instant::now();
    let measurement = qualify::run(Path::new(WORKER), &model_path(), model(), |phase| {
        phases.push(phase)
    })
    .expect("qualification must complete");
    println!(
        "qualification: {measurement:?} in {} ms",
        started.elapsed().as_millis()
    );
    assert_eq!(phases.len(), 4);
    let profile = model().profile;
    assert_eq!(measurement.tokenizer_cases, qualify::parity_count(profile));
    assert_eq!(measurement.tokenizer_mismatches, 0);
    assert_eq!(measurement.ranking_cases, qualify::ranking_count(profile));
    assert_eq!(measurement.unstable, 0);
    assert_eq!(measurement.bound_failures, 0);
    assert_eq!(measurement.verdict(profile), Reason::None);
    // The fixture count and universal coverage are part of the Tsumugi contract.
    assert_eq!(
        (
            measurement.tokenizer_cases,
            measurement.ranking_cases,
            measurement.ranking_unscored
        ),
        (373, 85, 0)
    );
}

/// Compare the F16 worker with reference scores from the training implementation.
#[test]
#[ignore = "needs KANADE_LLM_MODEL and the worker built with --features llama"]
fn tsumugi_scores_match_the_training_implementation() {
    type Case = (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static [&'static str],
        &'static [u32],
    );
    let cases: [Case; 3] = [
        (
            "明日は市民ホールで有名な作家の",
            "こうえんをきく",
            "",
            "を聞く",
            &["公園", "講演", "公演", "後援"],
            &[3226221214, 3208464866, 3231962496, 3232679491],
        ),
        // A space in the context: mapped by the profile, as it was by the
        // engine before.
        (
            "今日は いい天気",
            "こうえんであそんだ",
            "",
            "で遊んだ",
            &["公園", "講演"],
            &[3218081206, 3238810439],
        ),
        (
            "彼は",
            "きょうははしをわたる",
            "今日は",
            "を渡る",
            &["橋", "箸", "端"],
            &[3227859595, 3222808210, 3235374125],
        ),
    ];
    let mut worker = Worker::spawn(Path::new(WORKER)).unwrap();
    worker
        .load(&model_path(), model(), limits::THREADS)
        .unwrap();
    let mut exact = 0;
    for (context, reading, prefix, suffix, candidates, expected) in cases {
        let job = Job {
            context: kanade_llm::context_window(context).unwrap(),
            reading: katakana(reading),
            prefix: prefix.into(),
            suffix: suffix.into(),
            candidates: candidates.iter().map(|c| c.to_string()).collect(),
        };
        let scores = worker
            .score(&job, limits::BUDGET_MS, Duration::from_secs(5))
            .unwrap();
        assert_eq!(scores.len(), expected.len());
        for (score, bits) in scores.iter().zip(expected) {
            let (score, before) = (score.unwrap(), f32::from_bits(*bits));
            // Another CPU may round differently; a changed prompt moves a
            // score by far more than this.
            assert!((score - before).abs() < 0.02, "{score} {before}");
            exact += (score.to_bits() == *bits) as u32;
        }
    }
    println!("scores identical to the bit: {exact} of 9");
}

#[test]
#[ignore = "needs KANADE_LLM_MODEL and the worker built with --features llama"]
fn the_worker_bounds_its_work_and_refuses_other_files() {
    let mut worker = Worker::spawn(Path::new(WORKER)).unwrap();
    // Scoring before a model is loaded is refused without ending the process
    // in an undefined state: the parent treats it as fatal and kills it.
    assert_eq!(
        worker.score(&job("今日は"), 100, Duration::from_secs(2)),
        Err(Failure::Refused(Fault::NotLoaded))
    );
    assert!(!worker.alive());

    // A file that is not a model, and a truncated copy of the real one.
    let dir = tempfile::tempdir().unwrap();
    let garbage = dir.path().join("garbage.gguf");
    std::fs::write(&garbage, b"GGUF\x03\0\0\0not a model").unwrap();
    let truncated = dir.path().join("truncated.gguf");
    let mut head = vec![0u8; 4 << 20];
    std::fs::File::open(model_path())
        .unwrap()
        .read_exact(&mut head)
        .unwrap();
    std::fs::write(&truncated, &head).unwrap();
    // Neither is the file registered for the model: refused by size before
    // the runtime parses a byte of it.
    for (path, fault) in [
        (garbage, Fault::ProfileMismatch),
        (truncated, Fault::ProfileMismatch),
        (dir.path().join("missing.gguf"), Fault::ModelUnreadable),
    ] {
        let mut worker = Worker::spawn(Path::new(WORKER)).unwrap();
        assert_eq!(
            worker.load(&path, model(), limits::THREADS),
            Err(Failure::Refused(fault))
        );
        assert!(!worker.alive());
    }

    let mut worker = Worker::spawn(Path::new(WORKER)).unwrap();
    let loaded = worker
        .load(&model_path(), model(), limits::THREADS)
        .unwrap();
    assert!(identity_matches(&loaded, model()));
    // The worker reports the profile it applied, from its own registry.
    let profile = model().profile;
    assert_eq!(loaded.profile_sha256, profile.digest());
    assert_eq!((loaded.start, loaded.end), (profile.start, profile.end));
    // A second load is refused; the identity cannot change under the parent.
    assert_eq!(
        worker.load(&model_path(), model(), limits::THREADS),
        Err(Failure::Refused(Fault::AlreadyLoaded))
    );

    let mut worker = Worker::spawn(Path::new(WORKER)).unwrap();
    worker
        .load(&model_path(), model(), limits::THREADS)
        .unwrap();
    // No budget: the decode stops and the worker stays usable.
    let started = Instant::now();
    assert_eq!(
        worker.score(&job("今日は"), 0, Duration::from_secs(2)),
        Err(Failure::Refused(Fault::Deadline))
    );
    assert!(started.elapsed() < Duration::from_millis(limits::HARD_TIMEOUT_MS));
    // More tokens than the fixed prompt limit: refused, not truncated.
    let oversized = Job {
        context: profile.oversize.repeat(limits::CONTEXT_CHARS),
        prefix: profile.oversize.repeat(limits::SURROUND_CHARS),
        ..job("今日は")
    };
    assert_eq!(
        worker.score(&oversized, limits::BUDGET_MS, Duration::from_secs(2)),
        Err(Failure::Refused(Fault::TooLong))
    );
    // Text the tokenizer cannot reproduce byte for byte is reported as such,
    // never silently shortened. Both types of space are representable.
    let tokens = worker
        .tokenize(
            &["a b".into(), "a\u{3000}b".into(), String::new()],
            Duration::from_secs(2),
        )
        .unwrap();
    assert_eq!(tokens[0].as_ref().map(Vec::len), Some(3));
    assert_eq!(tokens[1].as_ref().map(Vec::len), Some(3));
    assert_eq!(tokens[2], Some(vec![]));
    // Literal special-token spellings are ordinary text.
    let mut literal = job("今日は");
    literal.candidates[0] = "</s>".into();
    let scores = worker
        .score(&literal, limits::BUDGET_MS, Duration::from_secs(2))
        .unwrap();
    assert!(scores.iter().all(|score| score.is_some_and(f32::is_finite)));
    // A marker of the profile in any text is refused before anything is
    // written to the pipe; the worker stays usable.
    for marker in ["\u{E081}", "\u{E082}", "\u{E080}", "\u{E087}"] {
        let mut marked = job("今日は");
        marked.candidates[2].push_str(marker);
        assert_eq!(
            worker.score(&marked, limits::BUDGET_MS, Duration::from_secs(2)),
            Err(Failure::Refused(Fault::Reserved))
        );
        let mut marked = job(&format!("今日{marker}は"));
        marked.suffix = "を聞く".into();
        assert_eq!(
            worker.score(&marked, limits::BUDGET_MS, Duration::from_secs(2)),
            Err(Failure::Refused(Fault::Reserved))
        );
    }
    assert!(worker.alive());
    // Scores are deterministic.
    let first = worker
        .score(&job("今日は"), limits::BUDGET_MS, Duration::from_secs(2))
        .unwrap();
    let second = worker
        .score(&job("今日は"), limits::BUDGET_MS, Duration::from_secs(2))
        .unwrap();
    assert_eq!(first, second);
}

#[test]
#[ignore = "needs KANADE_LLM_MODEL and the worker built with --features llama"]
fn the_worker_emits_frames_only_and_never_echoes_input() {
    // Drive the process directly so that both output streams can be captured.
    let mut child = Command::new(WORKER)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut job = job(&format!("{CANARY}の"));
    job.candidates[0] = format!("{CANARY}園");
    // Sent straight to the process, past the parent's check: a reading with
    // a marker of the profile.
    let mut marked = job.clone();
    marked.candidates[1] = format!("{CANARY}\u{E082}演");
    for request in [
        Request::Load {
            path: model_path().to_str().unwrap().into(),
            threads: limits::THREADS,
            model: model().id.into(),
        },
        Request::Tokenize {
            id: 1,
            texts: vec![CANARY.into()],
        },
        Request::Score {
            id: 2,
            budget_ms: limits::BUDGET_MS,
            job: marked,
        },
        Request::Score {
            id: 3,
            budget_ms: limits::BUDGET_MS,
            job,
        },
        Request::Shutdown,
    ] {
        frame::write(&mut input, &request.encode()).unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    // The runtime's own diagnostics are suppressed entirely.
    assert!(output.stderr.is_empty());
    let mut stream = output.stdout.as_slice();
    let mut replies = Vec::new();
    while !stream.is_empty() {
        replies.push(Reply::decode(&frame::read(&mut stream).unwrap()).unwrap());
    }
    assert_eq!(replies.len(), 5);
    assert!(matches!(replies[0], Reply::Hello { .. }));
    assert!(matches!(replies[1], Reply::Loaded(_)));
    assert!(matches!(replies[2], Reply::Tokens { .. }));
    // The worker itself refuses the marked request and then serves the next.
    assert_eq!(
        replies[3],
        Reply::Error {
            id: 2,
            fault: Fault::Reserved
        }
    );
    assert!(matches!(&replies[4], Reply::Scores { id: 3, scores, .. } if scores.len() == 4));
    let canary = CANARY.as_bytes();
    assert!(!output.stdout.windows(canary.len()).any(|w| w == canary));
}

/// Sends one load request to a fresh process and returns its answer.
fn load_reply(path: &Path, model: &str) -> Reply {
    let mut child = Command::new(WORKER)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_clear()
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for request in [
        Request::Load {
            path: path.to_str().unwrap().into(),
            threads: limits::THREADS,
            model: model.into(),
        },
        Request::Shutdown,
    ] {
        frame::write(&mut input, &request.encode()).unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    let mut stream = output.stdout.as_slice();
    let hello = Reply::decode(&frame::read(&mut stream).unwrap()).unwrap();
    assert!(
        matches!(hello, Reply::Hello { protocol, ref runtime, accelerators: 0 }
            if protocol == frame::PROTOCOL && runtime == profile::RUNTIME)
    );
    let reply = Reply::decode(&frame::read(&mut stream).unwrap()).unwrap();
    assert!(stream.is_empty());
    reply
}

#[test]
#[ignore = "needs KANADE_LLM_MODEL and the worker built with --features llama"]
fn the_worker_loads_only_as_a_registered_model() {
    // A model the worker's build does not register has no profile: nothing
    // is loaded, whatever the file is.
    for unknown in ["unregistered-q5km", "unsupported-profile", "model"] {
        assert_eq!(
            load_reply(&model_path(), unknown),
            Reply::Error {
                id: 0,
                fault: Fault::UnknownModel
            }
        );
    }
    assert!(matches!(
        load_reply(&model_path(), model().id),
        Reply::Loaded(loaded) if identity_matches(&loaded, model())
    ));
}

fn wait_for_job(manager: &Manager) -> Status {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let status = manager.status();
        match status.job {
            Some(job) if matches!(job.phase, JobPhase::Done | JobPhase::Failed) => return status,
            _ => assert!(Instant::now() < deadline, "management job did not finish"),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn wait_until(manager: &Manager, ready: bool) -> Status {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status = manager.status();
        if status.ready == ready
            && !matches!(status.state, Lifecycle::Loading | Lifecycle::Qualifying)
        {
            return status;
        }
        assert!(Instant::now() < deadline, "status did not settle");
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn import(path: PathBuf) -> ImportRequest {
    ImportRequest {
        path,
        model: model().id.into(),
        license: model().license.into(),
        terms_sha256: profile::terms_sha256(model()),
        accept_license: true,
        accept_local_processing: true,
    }
}
fn files_contain(dir: &Path, needle: &[u8]) -> bool {
    std::fs::read_dir(dir).unwrap().flatten().any(|entry| {
        let path = entry.path();
        // The model itself is the pinned file; only records can leak input.
        path.extension().is_some_and(|e| e == "json")
            && std::fs::read(path)
                .unwrap()
                .windows(needle.len())
                .any(|w| w == needle)
    })
}

#[test]
#[ignore = "needs KANADE_LLM_MODEL and the worker built with --features llama"]
fn import_needs_consent_and_readiness_is_tied_to_every_identity() {
    let data = tempfile::tempdir().unwrap();
    let dir = data.path().join("llm");
    let worker = PathBuf::from(WORKER);
    let manager = Manager::open(dir.clone(), worker.clone(), false);
    let status = wait_until(&manager, false);
    assert_eq!(status.state, Lifecycle::NotInstalled);
    assert_eq!(status.runtime, "available");
    assert_eq!(status.catalog.len(), profile::models().count());
    assert_eq!(status.catalog[0].license, model().license);
    assert_eq!(status.catalog[0].profile, model().profile.id);
    assert_eq!(manager.qualify().result, StartResult::NotInstalled);
    assert_eq!(manager.remove(), RemoveResult::NotInstalled);

    // Consent must be explicit and must name the current terms.
    type Edit = fn(&mut ImportRequest);
    let refusals: [(Edit, StartResult); 6] = [
        (|r| r.accept_license = false, StartResult::ConsentRequired),
        (
            |r| r.accept_local_processing = false,
            StartResult::ConsentRequired,
        ),
        (
            |r| r.license = "Apache-2.0".into(),
            StartResult::ConsentRequired,
        ),
        (
            |r| r.terms_sha256 = "0".repeat(64),
            StartResult::ConsentRequired,
        ),
        (
            |r| r.model = "another-model".into(),
            StartResult::UnsupportedModel,
        ),
        (
            |r| r.path = "relative/model.gguf".into(),
            StartResult::Invalid,
        ),
    ];
    for (edit, expected) in refusals {
        let mut request = import(model_path());
        edit(&mut request);
        assert_eq!(manager.import(request).result, expected);
    }
    assert!(!dir.join("model.gguf").exists());

    // A different file of the right size is refused by digest and not kept.
    let wrong = data.path().join("wrong.gguf");
    let mut bytes = std::fs::read(model_path()).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 1;
    std::fs::write(&wrong, &bytes).unwrap();
    drop(bytes);
    assert_eq!(manager.import(import(wrong)).result, StartResult::Started);
    let status = wait_for_job(&manager);
    assert_eq!(status.job.unwrap().error, JobError::ModelMismatch);
    assert_eq!(status.state, Lifecycle::NotInstalled);
    assert!(!dir.join("model.gguf").exists() && !dir.join("model.import").exists());
    let small = data.path().join("small.gguf");
    std::fs::write(&small, b"GGUF").unwrap();
    assert_eq!(manager.import(import(small)).result, StartResult::Started);
    assert_eq!(
        wait_for_job(&manager).job.unwrap().error,
        JobError::ModelMismatch
    );

    // The real file: copied, verified, qualified.
    let started = manager.import(import(model_path()));
    assert_eq!(started.result, StartResult::Started);
    assert_eq!(
        manager.import(import(model_path())).result,
        StartResult::Busy
    );
    assert_eq!(manager.remove(), RemoveResult::Busy);
    let status = wait_for_job(&manager);
    let job = status.job.unwrap();
    assert_eq!(
        (job.error, job.phase, job.copied),
        (JobError::None, JobPhase::Done, 100)
    );
    assert_eq!(status.state, Lifecycle::Qualified);
    assert!(status.ready && manager.ready() && !status.enabled);
    let measured = status.qualification.unwrap().measurement;
    println!("qualified: {measured:?}");
    let installed = status.installed.unwrap();
    assert_eq!(installed.sha256, model().sha256);
    assert_eq!(installed.profile, Some(model().profile.id));
    assert_eq!(manager.service().serving(), None);

    // Ready but not enabled: the service accepts nothing and runs no worker.
    let service = manager.service();
    assert_eq!(service.submit(job_for_service()), None);
    assert!(!service.active());

    // Enabled: real scores arrive in the background, well inside the deadline.
    manager.set_enabled(true);
    let scores = score(&service);
    assert_eq!(qualify::best(&scores), Some(1));
    assert!(scores.iter().all(|score| score.is_some_and(f32::is_finite)));
    // The identical request is answered from memory.
    let scored = service.scored();
    assert_eq!(score(&service), scores);
    assert_eq!(service.scored(), scored);
    assert!(!files_contain(&dir, CANARY.as_bytes()));
    manager.set_enabled(false);
    assert_eq!(service.submit(job_for_service()), None);

    // A fresh process sees the same qualified state from the records alone.
    let reopened = Manager::open(dir.clone(), worker.clone(), true);
    assert!(wait_until(&reopened, true).ready);
    assert_eq!(score(&reopened.service()), scores);
    drop(reopened);

    let store = Store::new(dir.clone());
    let install = store.install().unwrap();
    let good = store.qualification().bound().unwrap().clone();
    assert_eq!(good.evidence.profile, model().profile.id);
    assert_eq!(good.evidence.profile_sha256, model().profile.sha256());
    let worker_sha256 = kanade_llm::store::sha256_file(&worker).unwrap();
    assert_eq!(good.verdict(&install, &worker_sha256), Reason::None);
    let reason_after = |edit: &dyn Fn(&mut serde_json::Value)| {
        let path = dir.join("qualification.json");
        let original = std::fs::read(&path).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
        edit(&mut value);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let manager = Manager::open(dir.clone(), worker.clone(), true);
        let status = wait_until(&manager, false);
        assert_eq!(manager.service().submit(job_for_service()), None);
        std::fs::write(&path, original).unwrap();
        (status.state, status.reason)
    };
    // Editing a count, or re-declaring success for another build, is refused.
    let installed = |reason| (Lifecycle::Installed, reason);
    assert_eq!(
        reason_after(&|v| v["evidence"]["measurement"]["ranking_top1"] = 85.into()),
        installed(Reason::ReportInvalid)
    );
    assert_eq!(
        reason_after(&|v| v["digest"] = "0".repeat(64).into()),
        installed(Reason::ReportInvalid)
    );
    assert_eq!(
        reason_after(&|v| v["evidence"]["passed"] = true.into()),
        installed(Reason::ReportMissing)
    );
    // A record of the format before profiles is bound to no profile. It is
    // refused, and measuring again with the real worker makes the model
    // ready without a new import or consent.
    {
        let path = dir.join("qualification.json");
        let original = std::fs::read(&path).unwrap();
        let mut legacy: serde_json::Value = serde_json::from_slice(&original).unwrap();
        legacy["evidence"]["schema"] = 1.into();
        for key in [
            "profile",
            "profile_version",
            "profile_sha256",
            "tokenizer_sha256",
        ] {
            legacy["evidence"].as_object_mut().unwrap().remove(key);
        }
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert_eq!(store.qualification(), Recorded::Unbound);
        let consent = std::fs::read(dir.join("install.json")).unwrap();
        let manager = Manager::open(dir.clone(), worker.clone(), true);
        let status = wait_until(&manager, false);
        assert_eq!(
            (status.state, status.reason),
            installed(Reason::ProfileChanged)
        );
        assert_eq!(manager.service().submit(job_for_service()), None);
        assert_eq!(manager.qualify().result, StartResult::Started);
        let status = wait_for_job(&manager);
        assert_eq!(status.job.unwrap().error, JobError::None);
        assert!(wait_until(&manager, true).ready);
        assert_eq!(std::fs::read(dir.join("install.json")).unwrap(), consent);
        assert_eq!(score(&manager.service()), scores);
        drop(manager);
        std::fs::write(&path, original).unwrap();
    }
    // Consistent records that describe other identities are refused as well.
    let mut other = good.evidence.clone();
    other.prompt_sha256 = "1".repeat(64);
    assert_eq!(
        other.seal().verdict(&install, &worker_sha256),
        Reason::PromptChanged
    );
    let mut other = good.evidence.clone();
    other.parity_sha256 = "1".repeat(64);
    assert_eq!(
        other.seal().verdict(&install, &worker_sha256),
        Reason::TokenizerMismatch
    );
    let mut other = good.evidence.clone();
    other.profile_sha256 = "1".repeat(64);
    assert_eq!(
        other.seal().verdict(&install, &worker_sha256),
        Reason::ProfileChanged
    );
    let mut other = good.evidence.clone();
    other.tokenizer_sha256 = "1".repeat(64);
    assert_eq!(
        other.seal().verdict(&install, &worker_sha256),
        Reason::TokenizerMismatch
    );
    let mut other = good.evidence.clone();
    other.runtime = "another runtime".into();
    assert_eq!(
        other.seal().verdict(&install, &worker_sha256),
        Reason::RuntimeChanged
    );
    let mut other = good.evidence.clone();
    other.model_sha256 = "1".repeat(64);
    assert_eq!(
        other.seal().verdict(&install, &worker_sha256),
        Reason::ModelChanged
    );
    let mut other = good.evidence.clone();
    other.measurement.tokenizer_mismatches = 1;
    assert_eq!(
        other.seal().verdict(&install, &worker_sha256),
        Reason::TokenizerMismatch
    );
    let mut other = good.evidence.clone();
    other.measurement.ranking_top1 = 10;
    assert_eq!(
        other.seal().verdict(&install, &worker_sha256),
        Reason::QualityGate
    );

    // Another worker build needs a new qualification.
    let rebuilt = data.path().join("kanade-llm-worker");
    let mut binary = std::fs::read(&worker).unwrap();
    binary.extend_from_slice(b"\0rebuilt");
    std::fs::write(&rebuilt, binary).unwrap();
    let manager = Manager::open(dir.clone(), rebuilt, true);
    assert_eq!(wait_until(&manager, false).reason, Reason::RuntimeChanged);
    let manager = Manager::open(dir.clone(), data.path().join("absent"), true);
    let status = wait_until(&manager, false);
    assert_eq!(
        (status.runtime, status.reason),
        ("missing", Reason::WorkerMissing)
    );
    assert_eq!(manager.qualify().result, StartResult::Started);
    assert_eq!(
        wait_for_job(&manager).job.unwrap().error,
        JobError::WorkerMissing
    );

    // A model file that changed after import is not served.
    let model_file = dir.join("model.gguf");
    let stamp = std::fs::metadata(&model_file).unwrap().modified().unwrap();
    // The imported file is read-only; change it the way another tool would.
    assert!(
        std::fs::File::options()
            .write(true)
            .open(&model_file)
            .is_err()
    );
    let mut permissions = std::fs::metadata(&model_file).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(&model_file, permissions).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&model_file)
        .unwrap()
        .set_modified(stamp + Duration::from_secs(5))
        .unwrap();
    let manager = Manager::open(dir.clone(), worker.clone(), true);
    assert_eq!(wait_until(&manager, false).reason, Reason::ModelChanged);
    assert_eq!(manager.qualify().result, StartResult::Started);
    assert_eq!(
        wait_for_job(&manager).job.unwrap().error,
        JobError::ModelMismatch
    );

    // Removal deletes the model and every record.
    assert_eq!(manager.remove(), RemoveResult::Removed);
    assert_eq!(manager.status().state, Lifecycle::NotInstalled);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}
fn job_for_service() -> Job {
    job(&format!("{CANARY}。明日は市民ホールで有名な作家の"))
}
fn score(service: &kanade_llm::Service) -> Vec<Option<f32>> {
    // The first request also waits for the worker to load the model.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let Some(id) = service.submit(job_for_service()) else {
            assert!(Instant::now() < deadline, "service did not start");
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        loop {
            match service.take(id) {
                Poll::Ready(scores) => return scores,
                Poll::Pending => std::thread::sleep(Duration::from_millis(2)),
                Poll::Gone => break,
            }
        }
        assert!(Instant::now() < deadline, "no score arrived");
    }
}

/// The same model file under a test-only profile: nothing a profile owns is
/// taken from the catalogue model's profile or from a global.
#[cfg(feature = "fixture-profiles")]
mod fixtures {
    use super::*;
    use kanade_llm::profile::fixture;

    fn loaded(model: &'static profile::Model) -> (Worker, kanade_llm::frame::Loaded) {
        let mut worker = Worker::spawn(Path::new(WORKER)).unwrap();
        let loaded = worker.load(&model_path(), model, limits::THREADS).unwrap();
        (worker, loaded)
    }
    fn scores(worker: &mut Worker, job: &Job) -> Result<Vec<Option<f32>>, Failure> {
        worker.score(job, limits::BUDGET_MS, Duration::from_secs(5))
    }

    #[test]
    #[ignore = "needs KANADE_LLM_MODEL and the worker built with --features llama,fixture-profiles"]
    fn the_worker_applies_the_profile_of_the_selected_model() {
        let (tsumugi, alt) = (model(), fixture::alt());
        let (mut first, reported_first) = loaded(tsumugi);
        let (mut second, reported_second) = loaded(alt);
        // One file, one vocabulary, two profiles as reported by the workers.
        assert_eq!(reported_first.model_sha256, reported_second.model_sha256);
        assert_eq!(reported_first.vocab_sha256, reported_second.vocab_sha256);
        assert_eq!(reported_first.profile_sha256, tsumugi.profile.digest());
        assert_eq!(reported_second.profile_sha256, alt.profile.digest());
        assert_ne!(
            reported_first.profile_sha256,
            reported_second.profile_sha256
        );
        assert_eq!((reported_first.start, reported_first.end), (None, 259));
        assert_eq!((reported_second.start, reported_second.end), (Some(2), 2));
        // A report is accepted for its own model only.
        assert!(identity_matches(&reported_second, alt));
        assert!(!identity_matches(&reported_second, tsumugi));
        assert!(!identity_matches(&reported_first, alt));

        // Markers, start and end token differ, so the same request scores
        // differently; each profile is deterministic on its own.
        let plain = job("今日は");
        let a = scores(&mut first, &plain).unwrap();
        let b = scores(&mut second, &plain).unwrap();
        assert!(a.iter().chain(&b).all(|s| s.is_some_and(f32::is_finite)));
        assert_ne!(a, b);
        assert_eq!(scores(&mut first, &plain).unwrap(), a);
        assert_eq!(scores(&mut second, &plain).unwrap(), b);

        // Reserved markers are each profile's own.
        let mut bracket = job("今日は");
        bracket.candidates[0] = "「公園".into();
        assert!(scores(&mut first, &bracket).unwrap()[0].is_some());
        assert_eq!(
            scores(&mut second, &bracket),
            Err(Failure::Refused(Fault::Reserved))
        );
        let mut private = job("今日は");
        private.candidates[0] = "\u{E082}公園".into();
        assert_eq!(
            scores(&mut first, &private),
            Err(Failure::Refused(Fault::Reserved))
        );
        assert!(scores(&mut second, &private).is_ok());

        // The first profile maps ASCII space to ideographic space. The second
        // retains it. The vocabulary can encode both forms.
        let mut spaced = job("今日は");
        spaced.candidates[0] = "公 園".into();
        assert!(scores(&mut first, &spaced).unwrap()[0].is_some());
        let kept = scores(&mut second, &spaced).unwrap();
        assert!(kept[0].is_some() && kept[1].is_some());
    }

    #[test]
    #[ignore = "needs KANADE_LLM_MODEL and the worker built with --features llama,fixture-profiles"]
    fn a_file_is_refused_under_a_model_it_is_not_registered_for() {
        let mut worker = Worker::spawn(Path::new(WORKER)).unwrap();
        assert_eq!(
            worker.load(&model_path(), fixture::other(), limits::THREADS),
            Err(Failure::Refused(Fault::ProfileMismatch))
        );
        assert!(!worker.alive());
        assert_eq!(
            qualify::run(Path::new(WORKER), &model_path(), fixture::other(), |_| {}),
            Err(qualify::Refusal::Identity)
        );
    }

    #[test]
    #[ignore = "needs KANADE_LLM_MODEL and the worker built with --features llama,fixture-profiles"]
    fn qualification_and_management_use_the_selected_profile() {
        let (tsumugi, alt) = (model(), fixture::alt());
        let measurement =
            qualify::run(Path::new(WORKER), &model_path(), alt, |_| {}).expect("must complete");
        println!("fixture profile: {measurement:?}");
        // The fixture's own corpora were measured, not the catalogue model's.
        assert_eq!(
            (measurement.tokenizer_cases, measurement.ranking_cases),
            (
                qualify::parity_count(alt.profile),
                qualify::ranking_count(alt.profile)
            )
        );
        assert_eq!(
            (measurement.tokenizer_cases, measurement.ranking_cases),
            (5, 3)
        );
        assert_eq!(measurement.tokenizer_mismatches, 0);
        assert_eq!(measurement.bound_failures, 0);
        assert_eq!(measurement.verdict(alt.profile), Reason::None);
        assert_eq!(
            measurement.verdict(tsumugi.profile),
            Reason::TokenizerMismatch
        );

        // Import as the fixture model: consent to its terms, its profile in
        // the records, its scores from the service.
        let data = tempfile::tempdir().unwrap();
        let dir = data.path().join("llm");
        let worker = PathBuf::from(WORKER);
        let manager = Manager::open(dir.clone(), worker.clone(), false);
        wait_until(&manager, false);
        let request = |terms: &profile::Model| ImportRequest {
            path: model_path(),
            model: alt.id.into(),
            license: terms.license.into(),
            terms_sha256: profile::terms_sha256(terms),
            accept_license: true,
            accept_local_processing: true,
        };
        // Consent to the catalogue model's terms is not consent to these,
        // although the file is the same.
        assert_eq!(
            manager.import(request(tsumugi)).result,
            StartResult::ConsentRequired
        );
        assert_eq!(manager.import(request(alt)).result, StartResult::Started);
        let status = wait_for_job(&manager);
        assert_eq!(status.job.unwrap().error, JobError::None);
        assert!(status.ready);
        let installed = status.installed.unwrap();
        assert_eq!(
            (installed.id.as_str(), installed.profile),
            (alt.id, Some(alt.profile.id))
        );
        let summary = status.qualification.unwrap();
        assert_eq!(summary.profile, alt.profile.id);
        assert_eq!(summary.measurement.ranking_cases, 3);
        manager.set_enabled(true);
        let service = manager.service();
        let under_alt = score(&service);
        assert_eq!(service.serving(), Some(alt.id));
        manager.set_enabled(false);
        drop(manager);

        // The records now claim the catalogue model for the same file, with
        // its valid consent. The measurement made under the fixture profile
        // does not make it ready.
        let store = Store::new(dir.clone());
        let mut install = store.install().unwrap();
        install.model = tsumugi.id.into();
        install.terms_sha256 = profile::terms_sha256(tsumugi);
        store.save_install(&install).unwrap();
        let manager = Manager::open(dir.clone(), worker.clone(), true);
        let status = wait_until(&manager, false);
        assert_eq!(
            (status.state, status.reason),
            (Lifecycle::Installed, Reason::ModelChanged)
        );
        assert_eq!(manager.service().submit(job_for_service()), None);
        // Measured again as that model, it is served with that model's
        // profile, and the scores are that profile's.
        assert_eq!(manager.qualify().result, StartResult::Started);
        assert_eq!(wait_for_job(&manager).job.unwrap().error, JobError::None);
        assert!(wait_until(&manager, true).ready);
        let under_tsumugi = score(&manager.service());
        assert_eq!(manager.service().serving(), Some(tsumugi.id));
        assert_ne!(under_alt, under_tsumugi);
        assert_eq!(qualify::best(&under_tsumugi), Some(1));
    }
}
