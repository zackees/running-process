#![cfg(feature = "async-process")]

//! #850: the sync `NativeProcess` API keeps working when it is called from
//! inside a Tokio runtime, on a worker thread and on a current-thread
//! runtime alike. Sync `wait` parks on the actor runtime through
//! `block_on_anywhere`, so nesting must neither panic ("cannot start a
//! runtime from within a runtime") nor deadlock.

use std::time::Duration;

use running_process::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};

const CHILD_EXIT_WAIT: Duration = Duration::from_secs(30);

fn exits_with(code: i32) -> NativeProcess {
    NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            format!("import sys, time; time.sleep(0.2); print('hi'); sys.exit({code})"),
        ]),
        cwd: None,
        env: None,
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Null,
        nice: None,
        address_space_limit_bytes: None,
    })
}

fn run_sync_lifecycle(code: i32) {
    let process = exits_with(code);
    process.start().expect("start");
    // Slow enough that the untimed wait goes through the actor, not the
    // short direct-poll window.
    assert_eq!(process.wait(Some(CHILD_EXIT_WAIT)).expect("wait"), code);
    assert_eq!(process.returncode(), Some(code));
    assert!(process.captured_stdout().iter().any(|line| line == b"hi"));
    process.close().expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_wait_inside_a_multi_thread_runtime_completes() {
    run_sync_lifecycle(3);
}

#[tokio::test(flavor = "current_thread")]
async fn sync_wait_inside_a_current_thread_runtime_completes() {
    run_sync_lifecycle(5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_untimed_wait_inside_a_runtime_completes() {
    let process = exits_with(0);
    process.start().expect("start");
    assert_eq!(process.wait(None).expect("wait"), 0);
    process.close().expect("close");
}
