//! Sync/async parity contracts for #875.
//!
//! Each test here is cited by a row in `docs/async_api_parity.toml`. The point
//! is not that the async method runs -- it is that it agrees with the sync
//! method it claims parity with, so the two surfaces cannot drift apart
//! silently.
#![cfg(all(feature = "async-process", feature = "pty"))]

use std::time::Duration;

use running_process::pty::{AsyncPtyProcess, NativePtyProcess};
use running_process::{
    AsyncProcess, CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode,
};

/// Generous on purpose: these assertions are about behaviour, never speed, and
/// a loaded parallel test run can take seconds just to get a child going.
const CHILD_EXIT_WAIT: Duration = Duration::from_secs(30);

fn sync_config(argv: Vec<String>, create_process_group: bool) -> ProcessConfig {
    ProcessConfig {
        command: CommandSpec::Argv(argv),
        cwd: None,
        env: None,
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    }
}

fn sync_long_running(create_process_group: bool) -> NativeProcess {
    #[cfg(windows)]
    let argv = vec![
        "cmd.exe".to_string(),
        "/C".to_string(),
        "ping -n 60 127.0.0.1 > NUL".to_string(),
    ];
    #[cfg(not(windows))]
    let argv = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "sleep 60".to_string(),
    ];
    NativeProcess::new(sync_config(argv, create_process_group))
}

fn sync_pty_echo() -> NativePtyProcess {
    #[cfg(windows)]
    let argv = vec!["cmd.exe".into(), "/C".into(), "echo parity".into()];
    #[cfg(not(windows))]
    let argv = vec!["/bin/sh".into(), "-c".into(), "printf parity".into()];
    NativePtyProcess::new(argv, None, None, 24, 80, None).expect("PTY configuration")
}

/// A child that exits immediately with a known code.
fn exit_with(code: i32) -> AsyncProcess {
    #[cfg(windows)]
    {
        AsyncProcess::new("cmd.exe")
            .arg("/C")
            .arg(format!("exit {code}"))
    }
    #[cfg(not(windows))]
    {
        AsyncProcess::new("/bin/sh")
            .arg("-c")
            .arg(format!("exit {code}"))
    }
}

/// A child that stays alive until it is signalled.
fn long_running() -> AsyncProcess {
    #[cfg(windows)]
    {
        AsyncProcess::new("cmd.exe")
            .arg("/C")
            .arg("ping -n 60 127.0.0.1 > NUL")
    }
    #[cfg(not(windows))]
    {
        AsyncProcess::new("/bin/sh").arg("-c").arg("sleep 60")
    }
}

fn pty_echo() -> AsyncPtyProcess {
    #[cfg(windows)]
    let argv = vec!["cmd.exe".into(), "/C".into(), "echo parity".into()];
    #[cfg(not(windows))]
    let argv = vec!["/bin/sh".into(), "-c".into(), "printf parity".into()];
    AsyncPtyProcess::new(argv, None, None, 24, 80, None).expect("async PTY configuration")
}

// ---------------------------------------------------------------------------
// AsyncProcess: lifecycle observation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn async_process_poll_reports_none_before_exit_and_a_status_after() {
    let mut process = long_running();
    process.start().await.expect("start");
    assert!(
        process.poll().await.expect("poll while running").is_none(),
        "poll must not report an exit status for a live child"
    );
    process.kill().await.expect("kill");
    process.wait().await.expect("wait");
    assert!(
        process.poll().await.expect("poll after exit").is_some(),
        "poll must report the status once the actor has observed the exit"
    );
}

#[tokio::test]
async fn async_process_returncode_matches_the_exit_code_the_child_chose() {
    let mut process = exit_with(3);
    process.start().await.expect("start");
    process.wait().await.expect("wait");
    assert_eq!(process.returncode().await.expect("returncode"), Some(3));
}

#[tokio::test]
async fn async_process_returncode_is_none_while_the_child_is_running() {
    // The sync `returncode` never blocks; neither may the async one.
    let mut process = long_running();
    process.start().await.expect("start");
    assert_eq!(process.returncode().await.expect("returncode"), None);
    process.kill().await.expect("kill");
}

#[tokio::test]
async fn async_process_poll_before_start_reports_not_running() {
    let mut process = exit_with(0);
    assert!(process.poll().await.is_err());
    assert!(process.returncode().await.is_err());
}

#[tokio::test]
async fn async_process_terminate_ends_the_child_like_kill() {
    let mut process = long_running();
    process.start().await.expect("start");
    process.terminate().await.expect("terminate");
    let status = process.wait().await.expect("wait");
    assert!(
        !status.success(),
        "a terminated child must not report success"
    );
}

#[tokio::test]
async fn async_process_close_releases_the_actor_and_is_idempotent() {
    let mut process = exit_with(0);
    process.start().await.expect("start");
    process.close().await.expect("first close");
    process.close().await.expect("second close");
    // After close the handle owns no actor, so lifecycle queries fail rather
    // than silently answering from a stale cache.
    assert!(process.wait().await.is_err());
}

#[tokio::test]
async fn async_process_close_before_start_succeeds() {
    let mut process = exit_with(0);
    process
        .close()
        .await
        .expect("close on an unstarted process");
}

// ---------------------------------------------------------------------------
// AsyncProcess: process group and tree
// ---------------------------------------------------------------------------

#[tokio::test]
async fn terminate_group_soft_is_a_noop_without_an_owned_process_group() {
    // Mirrors the sync contract: no group was created, so there is nothing
    // addressable and the call must not signal the caller's own group.
    let mut process = long_running();
    process.start().await.expect("start");
    assert!(
        !process
            .terminate_group_soft()
            .await
            .expect("terminate_group_soft"),
        "without create_process_group there is no group to signal"
    );
    process.kill().await.expect("kill");
}

#[tokio::test]
async fn terminate_group_soft_signals_a_group_the_child_owns() {
    let mut process = long_running().create_process_group(true);
    process.start().await.expect("start");
    assert!(
        process
            .terminate_group_soft()
            .await
            .expect("terminate_group_soft"),
        "an owned process group must be signalled"
    );

    // On POSIX SIGTERM ends `sleep`, so the graceful step alone is enough and
    // the exit is contracted.
    #[cfg(not(windows))]
    {
        let status = tokio::time::timeout(Duration::from_secs(10), process.wait())
            .await
            .expect("the group signal must end the child")
            .expect("wait");
        assert!(!status.success());
    }
    // On Windows CTRL_BREAK is advisory: whether it ends the child depends on
    // the console the harness gives us, so only delivery is contracted above.
    // Cleanup is best-effort for the same reason -- when the break *did* end
    // the child, the follow-up kill sees ACCESS_DENIED on an exited pid.
    #[cfg(windows)]
    {
        let _ = tokio::time::timeout(Duration::from_secs(10), process.wait()).await;
        let _ = process.kill().await;
    }
}

#[tokio::test]
async fn terminate_group_soft_after_exit_reports_no_group_signalled() {
    let mut process = exit_with(0).create_process_group(true);
    process.start().await.expect("start");
    process.wait().await.expect("wait");
    assert!(
        !process
            .terminate_group_soft()
            .await
            .expect("terminate_group_soft"),
        "an exited child has nothing left to ask nicely"
    );
}

#[tokio::test]
async fn terminate_group_soft_before_start_reports_not_running() {
    let mut process = long_running().create_process_group(true);
    assert!(process.terminate_group_soft().await.is_err());
}

#[tokio::test]
async fn async_kill_tree_terminates_the_started_child() {
    let mut process = long_running();
    process.start().await.expect("start");
    let killed = process
        .kill_tree(Duration::from_secs(5))
        .await
        .expect("kill_tree");
    assert!(
        killed >= 1,
        "kill_tree must report at least the root instance, reported {killed}"
    );
    let status = tokio::time::timeout(Duration::from_secs(10), process.wait())
        .await
        .expect("kill_tree must end the child")
        .expect("wait");
    assert!(!status.success());
}

#[tokio::test]
async fn async_kill_tree_before_start_reports_not_running() {
    let mut process = long_running();
    assert!(process.kill_tree(Duration::from_secs(1)).await.is_err());
}

// ---------------------------------------------------------------------------
// AsyncPtyProcess: signal, tree, idle, relay, metrics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn async_pty_send_interrupt_before_start_errors() {
    // Same precondition as the sync `send_interrupt_impl`: no child, no signal.
    let process = pty_echo();
    assert!(process.send_interrupt().await.is_err());
}

#[tokio::test]
async fn async_pty_respond_to_queries_without_a_query_is_a_noop() {
    // Matches the sync contract: the call scans the chunk for query escapes
    // and only writes when it finds one, so a chunk with nothing to answer
    // succeeds without needing a started child.
    let process = pty_echo();
    process
        .respond_to_queries(b"plain output".to_vec())
        .await
        .expect("respond_to_queries with no query present");
}

#[tokio::test]
async fn async_pty_tree_termination_dispatches_through_the_island() {
    let process = pty_echo();
    process.start().await.expect("start");
    process.terminate_tree().await.expect("terminate_tree");
    process.kill_tree().await.expect("kill_tree");
    process.close().await.expect("close");
}

#[tokio::test]
async fn async_pty_wait_and_drain_agrees_with_wait_on_an_exited_child() {
    // Scoped deliberately to a child that has already exited.
    //
    // `wait_and_drain` holds one of the two island permits for its entire
    // blocking duration, so a variant that waits on a *live* PTY child can
    // starve every other PTY operation in the process -- including the reads
    // that child needs before it can exit. The pump-while-waiting shape is
    // covered on the sync side by
    // `interactive_pty_session_pumps_output_and_waits_for_exit`, which owns its
    // own pump thread. What is contracted here is the async wrapper: that it
    // dispatches, bounds itself, and reports the same code `wait` does.
    let process = pty_echo();
    process.start().await.expect("start");
    process.kill().await.expect("kill");

    let code = process
        .wait_and_drain(Some(Duration::from_secs(20)), Duration::from_secs(1))
        .await
        .expect("wait_and_drain");
    let waited = process
        .wait(Some(Duration::from_secs(5)))
        .await
        .expect("wait after wait_and_drain");
    // Agreement, not a literal code: what a killed PTY child reports differs
    // by platform, and that is not what this contract is about.
    assert_eq!(
        code, waited,
        "wait_and_drain must report the same exit code as wait"
    );
    process.close().await.expect("close");
}

#[tokio::test]
async fn async_pty_wait_for_reader_closed_reports_a_bounded_timeout() {
    let process = pty_echo();
    process.start().await.expect("start");
    // Whether the reader has closed by now is timing-dependent; what is
    // contracted is that the call returns within its bound instead of hanging.
    let closed = tokio::time::timeout(
        Duration::from_secs(20),
        process.wait_for_reader_closed(Some(Duration::from_millis(200))),
    )
    .await
    .expect("wait_for_reader_closed must respect its own timeout")
    .expect("wait_for_reader_closed");
    let _ = closed;
    process.close().await.expect("close");
}

#[tokio::test]
async fn async_pty_echo_state_round_trips() {
    let process = pty_echo();
    let initial = process.echo_enabled();
    process.set_echo(!initial);
    assert_eq!(process.echo_enabled(), !initial);
    process.set_echo(initial);
    assert_eq!(process.echo_enabled(), initial);
}

#[tokio::test]
async fn async_pty_relay_is_inactive_until_started_and_stops_cleanly() {
    let process = pty_echo();
    assert!(!process.terminal_input_relay_active());
    // Requesting a stop for a relay that never ran is a no-op, not an error:
    // teardown paths call it unconditionally.
    process.request_terminal_input_relay_stop();
    process
        .stop_terminal_input_relay()
        .await
        .expect("stop_terminal_input_relay");
    assert!(!process.terminal_input_relay_active());
}

#[tokio::test]
async fn async_pty_start_terminal_input_relay_requires_a_running_pty() {
    let process = pty_echo();
    assert!(process.start_terminal_input_relay().await.is_err());
}

#[tokio::test]
async fn async_pty_metrics_track_recorded_input_like_the_sync_counters() {
    let process = pty_echo();
    assert_eq!(process.pty_input_bytes_total(), 0);
    assert_eq!(process.pty_newline_events_total(), 0);
    assert_eq!(process.pty_submit_events_total(), 0);

    process.record_input_metrics(b"abc\n", true);

    assert_eq!(process.pty_input_bytes_total(), 4);
    assert_eq!(process.pty_newline_events_total(), 1);
    assert_eq!(process.pty_submit_events_total(), 1);
    // Output counters are fed by the reader, not by input accounting.
    assert_eq!(process.pty_output_bytes_total(), 0);
    assert_eq!(process.pty_control_churn_bytes_total(), 0);
}

#[tokio::test]
async fn async_pty_store_returncode_and_mark_reader_closed_are_observable() {
    let process = pty_echo();
    process.store_returncode(7);
    process.mark_reader_closed();
    // A closed reader must not make the bounded wait hang.
    assert!(process
        .wait_for_reader_closed(Some(Duration::from_millis(200)))
        .await
        .expect("wait_for_reader_closed"));
}

#[tokio::test]
async fn async_pty_close_nonblocking_is_safe_before_start() {
    let process = pty_echo();
    process.close_nonblocking();
    process.close_nonblocking();
}

#[tokio::test]
async fn async_pty_idle_detector_attaches_detaches_and_reports_an_outcome() {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Instant;

    use running_process::pty::{IdleDetectorCore, IdleMonitorState};

    let process = pty_echo();
    let detector = Arc::new(IdleDetectorCore {
        timeout_seconds: 0.05,
        stability_window_seconds: 0.0,
        sample_interval_seconds: 0.01,
        reset_on_input: true,
        reset_on_output: true,
        count_control_churn_as_output: false,
        enabled: Arc::new(AtomicBool::new(true)),
        state: Mutex::new(IdleMonitorState {
            last_reset_at: Instant::now(),
            returncode: None,
            interrupted: false,
        }),
        condvar: Condvar::new(),
    });
    process
        .attach_idle_detector(Arc::clone(&detector))
        .await
        .expect("attach_idle_detector");

    let outcome = process
        .wait_for_idle(Arc::clone(&detector), Some(Duration::from_secs(2)))
        .await
        .expect("wait_for_idle");
    assert!(
        outcome.idle_seconds >= 0.0,
        "an idle wait must report how long the PTY was idle"
    );

    process
        .detach_idle_detector()
        .await
        .expect("detach_idle_detector");
}

// ---------------------------------------------------------------------------
// Sync counterparts
//
// Each of these is the sync half of a parity row whose async half is above.
// They exist so both columns of a row cite a contract written against the same
// expectation, rather than pairing a purpose-built async test with whatever
// pre-existing sync test happened to touch the same method.
// ---------------------------------------------------------------------------

#[test]
fn sync_process_poll_reports_none_before_exit_and_a_code_after() {
    let process = sync_long_running(false);
    process.start().expect("start");
    assert_eq!(process.poll().expect("poll while running"), None);
    process.kill().expect("kill");
    process.wait(Some(CHILD_EXIT_WAIT)).expect("wait");
    assert!(process.poll().expect("poll after exit").is_some());
}

#[test]
fn sync_process_returncode_is_none_until_the_child_exits() {
    let process = sync_long_running(false);
    process.start().expect("start");
    assert_eq!(process.returncode(), None);
    process.kill().expect("kill");
    process.wait(Some(CHILD_EXIT_WAIT)).expect("wait");
    assert!(process.returncode().is_some());
}

#[test]
fn sync_process_terminate_ends_the_child_like_kill() {
    let process = sync_long_running(false);
    process.start().expect("start");
    process.terminate().expect("terminate");
    let code = process.wait(Some(CHILD_EXIT_WAIT)).expect("wait");
    assert_ne!(code, 0, "a terminated child must not report success");
}

#[test]
fn sync_process_close_is_idempotent() {
    let process = sync_long_running(false);
    process.start().expect("start");
    process.kill().expect("kill");
    process.wait(Some(CHILD_EXIT_WAIT)).expect("wait");
    process.close().expect("first close");
    process.close().expect("second close");
}

#[test]
fn sync_terminate_group_soft_is_a_noop_without_an_owned_process_group() {
    // The async surface reports this as `false`; the sync surface predates
    // that return value and reports `Ok(())`. Both mean "nothing signalled".
    let process = sync_long_running(false);
    process.start().expect("start");
    process
        .terminate_group_soft()
        .expect("terminate_group_soft is a no-op without a group");
    assert_eq!(
        process.poll().expect("poll"),
        None,
        "a no-op must not have ended the child"
    );
    process.kill().expect("kill");
    process.wait(Some(CHILD_EXIT_WAIT)).expect("wait");
}

#[test]
fn sync_terminate_group_soft_signals_a_group_the_child_owns() {
    let process = sync_long_running(true);
    process.start().expect("start");
    process
        .terminate_group_soft()
        .expect("terminate_group_soft on an owned group");

    #[cfg(not(windows))]
    {
        let code = process
            .wait(Some(CHILD_EXIT_WAIT))
            .expect("the group signal must end the child");
        assert_ne!(code, 0);
    }
    // Advisory on Windows -- see the async counterpart for why cleanup is
    // best-effort here.
    #[cfg(windows)]
    {
        let _ = process.wait(Some(Duration::from_secs(10)));
        let _ = process.kill();
    }
}

#[test]
fn sync_terminate_group_soft_before_start_reports_not_running() {
    let process = sync_long_running(true);
    assert!(process.terminate_group_soft().is_err());
}

#[test]
fn sync_kill_tree_terminates_the_started_child() {
    let process = sync_long_running(false);
    process.start().expect("start");
    let pid = process.pid().expect("pid after start");
    let killed =
        running_process::process_tree::kill_tree(pid, Duration::from_secs(5)).expect("kill_tree");
    assert!(killed >= 1, "kill_tree reported {killed} instances");
    let code = process.wait(Some(CHILD_EXIT_WAIT)).expect("wait");
    assert_ne!(code, 0);
}

#[test]
fn sync_pty_send_interrupt_before_start_errors() {
    let process = sync_pty_echo();
    assert!(process.send_interrupt_impl().is_err());
}

#[test]
fn sync_pty_respond_to_queries_without_a_query_is_a_noop() {
    let process = sync_pty_echo();
    process
        .respond_to_queries_impl(b"plain output")
        .expect("respond_to_queries with no query present");
}

#[test]
fn sync_pty_tree_termination_is_accepted_after_start() {
    let process = sync_pty_echo();
    process.start_impl().expect("start");
    process.terminate_tree_impl().expect("terminate_tree");
    process.kill_tree_impl().expect("kill_tree");
    process.close_impl().expect("close");
}

#[test]
fn sync_pty_wait_and_drain_agrees_with_wait_on_an_exited_child() {
    let process = sync_pty_echo();
    process.start_impl().expect("start");
    process.kill_impl().expect("kill");
    let code = process
        .wait_and_drain_impl(Some(20.0), 1.0)
        .expect("wait_and_drain");
    let waited = process.wait_impl(Some(5.0)).expect("wait");
    assert_eq!(code, waited);
    process.close_impl().expect("close");
}

#[test]
fn sync_pty_wait_for_reader_closed_reports_a_bounded_timeout() {
    let process = sync_pty_echo();
    process.start_impl().expect("start");
    let _ = process.wait_for_reader_closed_impl(Some(0.2));
    process.close_impl().expect("close");
}

#[test]
fn sync_pty_echo_state_round_trips() {
    let process = sync_pty_echo();
    let initial = process.echo_enabled();
    process.set_echo(!initial);
    assert_eq!(process.echo_enabled(), !initial);
    process.set_echo(initial);
    assert_eq!(process.echo_enabled(), initial);
}

#[test]
fn sync_pty_relay_is_inactive_until_started_and_stops_cleanly() {
    let process = sync_pty_echo();
    assert!(!process.terminal_input_relay_active());
    process.request_terminal_input_relay_stop();
    process.stop_terminal_input_relay_impl();
    assert!(!process.terminal_input_relay_active());
}

#[test]
fn sync_pty_start_terminal_input_relay_requires_a_running_pty() {
    let process = sync_pty_echo();
    assert!(process.start_terminal_input_relay_impl().is_err());
}

#[test]
fn sync_pty_metrics_track_recorded_input() {
    let process = sync_pty_echo();
    assert_eq!(process.pty_input_bytes_total(), 0);
    process.record_input_metrics(b"abc\n", true);
    assert_eq!(process.pty_input_bytes_total(), 4);
    assert_eq!(process.pty_newline_events_total(), 1);
    assert_eq!(process.pty_submit_events_total(), 1);
    assert_eq!(process.pty_output_bytes_total(), 0);
    assert_eq!(process.pty_control_churn_bytes_total(), 0);
}

#[test]
fn sync_pty_store_returncode_and_mark_reader_closed_are_observable() {
    let process = sync_pty_echo();
    process.store_returncode(7);
    process.mark_reader_closed();
    assert!(process.wait_for_reader_closed_impl(Some(0.2)));
}

#[test]
fn sync_pty_close_nonblocking_is_safe_before_start() {
    let process = sync_pty_echo();
    process.close_nonblocking();
    process.close_nonblocking();
}

#[test]
fn sync_pty_idle_detector_attaches_and_detaches() {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Instant;

    use running_process::pty::{IdleDetectorCore, IdleMonitorState};

    let process = sync_pty_echo();
    let detector = Arc::new(IdleDetectorCore {
        timeout_seconds: 0.05,
        stability_window_seconds: 0.0,
        sample_interval_seconds: 0.01,
        reset_on_input: true,
        reset_on_output: true,
        count_control_churn_as_output: false,
        enabled: Arc::new(AtomicBool::new(true)),
        state: Mutex::new(IdleMonitorState {
            last_reset_at: Instant::now(),
            returncode: None,
            interrupted: false,
        }),
        condvar: Condvar::new(),
    });
    process.attach_idle_detector(&detector);
    let (_reached, _reason, idle_seconds, _code) = detector.wait(Some(2.0));
    assert!(idle_seconds >= 0.0);
    process.detach_idle_detector();
}
