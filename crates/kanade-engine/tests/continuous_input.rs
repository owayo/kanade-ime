//! Black-box continuity with a slow but healthy real conversion worker.
#![cfg(unix)]

use kanade_ipc::{frontend::*, unix::DeadlineStream, *};
use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct EngineProcess(Child);
impl Drop for EngineProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Default)]
struct Ledger {
    allow_commit: bool,
    commits: usize,
    replies: usize,
    connection: Option<u64>,
    value: String,
}
impl Document for Ledger {
    fn apply(&mut self, reply: &KeyReply) -> bool {
        let connection = reply.revision.as_ref().unwrap().connection;
        if let Some(previous) = self.connection {
            assert_eq!(connection, previous, "a healthy connection was replaced");
        }
        self.connection = Some(connection);
        self.replies += 1;
        if let Some(commit) = &reply.commit {
            assert!(self.allow_commit, "composition committed without Enter");
            self.commits += 1;
            self.value.push_str(&commit.value);
        }
        true
    }
    fn finish_reading(&mut self, _: &str) -> bool {
        panic!("a healthy slow worker must not trigger fallback insertion");
    }
}

// Opaque frame forwarding keeps upstream protocol types private to kanade-mozc.
// The proxy and real worker only receive authored fixture input in a temporary
// profile. Its only diagnostic file contains a process ID, never a frame.
const PROXY: &str = r#"#!/usr/bin/env python3
import os
import struct
import subprocess
import sys
import time

def exact(stream, count):
    result = bytearray()
    while len(result) < count:
        chunk = stream.read(count - len(result))
        if not chunk:
            return None
        result.extend(chunk)
    return bytes(result)

def frame(stream):
    header = exact(stream, 4)
    if header is None:
        return None
    size = struct.unpack('<I', header)[0]
    if size > 1 << 20:
        raise RuntimeError('oversized fixture frame')
    body = exact(stream, size)
    return None if body is None else header + body

worker = subprocess.Popen([os.environ['KANADE_REAL_WORKER'], sys.argv[1]],
                          stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                          stderr=subprocess.DEVNULL)
with open(os.environ['KANADE_PROXY_PID_FILE'], 'w') as output:
    output.write(str(worker.pid))
try:
    while True:
        request = frame(sys.stdin.buffer)
        if request is None:
            break
        time.sleep(int(os.environ['KANADE_PROXY_DELAY_MS']) / 1000)
        worker.stdin.write(request)
        worker.stdin.flush()
        reply = frame(worker.stdout)
        if reply is None:
            break
        sys.stdout.buffer.write(reply)
        sys.stdout.buffer.flush()
finally:
    worker.stdin.close()
    try:
        worker.wait(timeout=2)
    except subprocess.TimeoutExpired:
        worker.kill()
        worker.wait()
"#;

#[test]
#[ignore = "requires the locked Mozc worker and python3; serial stability contract"]
fn healthy_delayed_worker_keeps_composition_connection_and_process() {
    // Establish that the opaque proxy itself preserves ordinary conversion
    // before introducing a delay above the earlier worker timeout.
    check_continuity(0, 1);
    check_continuity(65, 4);
}

fn check_continuity(delay_ms: u32, repetitions: usize) {
    println!("continuity probe: delay={delay_ms}ms repetitions={repetitions}");
    let temporary = tempfile::Builder::new()
        .prefix("kanade-continuity-")
        .tempdir_in("/tmp")
        .unwrap();
    let proxy = temporary.path().join("delayed-worker");
    let worker_pid = temporary.path().join("worker-pid");
    std::fs::write(&proxy, PROXY).unwrap();
    std::fs::set_permissions(&proxy, std::fs::Permissions::from_mode(0o700)).unwrap();
    let real_worker = kanade_mozc::default_worker();
    kanade_mozc::verify_artifact(&real_worker).unwrap();
    // The fixture has its own digest. Verify the actual upstream worker
    // independently before describing the transparent proxy as an artifact.
    let mut manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(real_worker.with_file_name("manifest.json")).unwrap(),
    )
    .unwrap();
    let digest = Command::new("python3")
        .args(["-c", "import hashlib,pathlib,sys; print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())"])
        .arg(&proxy).output().unwrap();
    assert!(digest.status.success());
    manifest["sha256"] = String::from_utf8(digest.stdout).unwrap().trim().into();
    std::fs::write(
        temporary.path().join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let mut server = EngineProcess(
        Command::new(env!("CARGO_BIN_EXE_kanade-engine"))
            .env("TMPDIR", temporary.path())
            .env("KANADE_DATA_DIR", temporary.path().join("data"))
            .env("KANADE_MOZC_WORKER", &proxy)
            .env("KANADE_REAL_WORKER", real_worker)
            .env("KANADE_PROXY_PID_FILE", &worker_pid)
            .env("KANADE_PROXY_DELAY_MS", delay_ms.to_string())
            .env("KANADE_ALLOW_UNSIGNED", "1")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let socket = temporary.path().join("kanade-ime/engine.sock");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "engine exited during healthy delayed startup"
        );
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "engine did not open its socket");
        std::thread::sleep(Duration::from_millis(10));
    }
    let worker_before = std::fs::read_to_string(&worker_pid).unwrap();
    let stream = DeadlineStream::connect(Path::new(&socket), KEY_TIMEOUT).unwrap();
    let mut client = Client::connect(stream).expect("healthy delayed session must open");
    let mut ledger = Ledger::default();
    for _ in 0..repetitions {
        for text in ["k", "a", "n", "a", "d", "e"] {
            assert!(
                client.key(
                    Key {
                        text: text.into(),
                        ..Default::default()
                    },
                    Context {
                        privacy: Privacy::Private as i32,
                        ..Default::default()
                    },
                    false,
                    &mut ledger,
                ),
                "authored input must stay in the IME"
            );
        }
        assert!(client.is_connected());
        // Leave marked text untouched between frames. The server must not
        // mistake inactivity for a partial request or a dead peer.
        std::thread::sleep(Duration::from_millis(150));
        ledger.allow_commit = true;
        assert!(client.key(
            Key {
                special: SpecialKey::Enter as i32,
                ..Default::default()
            },
            Context {
                privacy: Privacy::Private as i32,
                ..Default::default()
            },
            false,
            &mut ledger,
        ));
        ledger.allow_commit = false;
    }
    assert!(server.0.try_wait().unwrap().is_none());
    assert!(client.is_connected());
    assert_eq!(
        std::fs::read_to_string(worker_pid).unwrap(),
        worker_before,
        "healthy worker was restarted"
    );
    assert_eq!(ledger.commits, repetitions);
    assert_eq!(ledger.replies, repetitions * 7);
    assert_eq!(ledger.value, "かなで".repeat(repetitions));
}
