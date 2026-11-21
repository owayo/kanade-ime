// SPDX-License-Identifier: MIT
//! A panic leaves a record of where it started and nothing of what it said.
//! The hook and the writer belong to the process, so this has its own binary.
use kanade_ipc::diagnostics::{self, Component, Thread};
use std::time::Duration;

#[test]
fn a_panic_is_recorded_by_thread_and_position_without_its_message() {
    let directory = std::env::temp_dir().join(format!("kanade-panic-{}", std::process::id()));
    diagnostics::start(Component::Engine, Some(&directory));
    diagnostics::record_panics();
    // The message quotes what was being processed, as a slicing error does.
    let reading = "ひみつのよみ";
    let named = line!() + 4;
    let connection = std::thread::Builder::new()
        .name(Thread::CONNECTION.into())
        .spawn(move || {
            panic!("byte index 4 is not a char boundary of `{reading}`");
        })
        .unwrap();
    assert!(connection.join().is_err());
    let unnamed = line!() + 2;
    let other = std::thread::spawn(|| {
        let _: u32 = "not a number".parse().unwrap();
    });
    assert!(other.join().is_err());
    diagnostics::flush(Duration::from_secs(5));
    let log = std::fs::read_to_string(directory.join("engine-events.log")).unwrap();
    std::fs::remove_dir_all(&directory).unwrap();
    let records: Vec<_> = log
        .lines()
        .map(|line| line.splitn(3, ' ').nth(2).unwrap())
        .collect();
    assert_eq!(
        records,
        [
            format!("panicked thread=connection at=panic_record.rs:{named}"),
            // A thread this project did not name is not named in the log.
            format!("panicked thread=other at=panic_record.rs:{unnamed}"),
        ]
    );
    for quoted in [reading, "char boundary", "not a number", "ParseIntError"] {
        assert!(!log.contains(quoted), "{quoted}");
    }
}
