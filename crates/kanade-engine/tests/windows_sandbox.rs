// SPDX-License-Identifier: MIT
//! Frontends inside an AppContainer against the engine on Windows.
//!
//! An AppContainer is the sandbox of packaged applications and of the
//! system's own search and settings windows. A text service is loaded into
//! those hosts like into any other. The test runs a frontend in a process of
//! its own inside such a sandbox, made without registering anything, against
//! an engine on a private endpoint with a temporary data directory.
#![cfg(all(windows, feature = "fault-injection"))]
mod windows_support;

use std::{ffi::OsStr, time::Duration};
use windows_support::{Engine, engine_binary, sandbox, stand_in_binary};

#[test]
#[ignore = "requires the locked worker and a debug engine; cargo xtask build-worker"]
fn a_frontend_in_an_app_container_connects_and_types() {
    let engine = Engine::start("sandbox");
    // The frontend exits with 0 when its word was committed, and with 1000
    // plus the system's error when it could not connect at all.
    let typed = sandbox::run(
        &stand_in_binary(),
        &[
            OsStr::new("type"),
            OsStr::new(&engine.endpoint),
            engine_binary().as_os_str(),
        ],
        Duration::from_secs(20),
    )
    .unwrap();
    // A frontend connects only after it has checked which program serves
    // the pipe. A sandboxed one may not open another process, so the engine
    // lets packages identify it. Without that this is 1005, access denied:
    // the pipe opens, the check fails, and such a host never converts.
    assert_eq!(typed, Some(0), "events: {:?}", engine.events());
    let events = engine.events();
    for refusal in ["connection-rejected", "sandboxed-hosts-unreachable"] {
        assert!(
            !events.iter().any(|event| event.starts_with(refusal)),
            "{events:?}"
        );
    }
    assert!(engine.running());
}
