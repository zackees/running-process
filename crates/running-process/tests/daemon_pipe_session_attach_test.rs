//! Integration test for daemon-owned pipe-backed sessions
//! (#130 milestone 3).
//!
//! Parallel to `pty_session_attach_test.rs` but for pipe sessions. Uses
//! two independent OS-level socket clients against an in-process
//! `DaemonServer` to validate spawn → list → attach to stdout → terminate.

use running_process::daemon::client::DaemonClient;
use running_process::daemon::paths;
use running_process::daemon::pipe_session::{PipeSpawnRequest, PipeStreamAttachment};
use running_process::daemon::server::DaemonServer;
use running_process::proto::daemon::PipeStreamKind;

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

fn testbin_path(name: &str) -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .args(["build", "-p", "testbins", "--bin", name, "--message-format=json"])
        .stderr(std::process::Stdio::inherit())
        .output()
        .expect("cargo build failed");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if !line.contains("\"compiler-artifact\"") || !line.contains(name) {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v["reason"] == "compiler-artifact"
                && v["target"]["kind"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|k| k == "bin"))
            {
                if let Some(exe) = v["executable"].as_str() {
                    let p = PathBuf::from(exe);
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !p.exists() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    return p;
                }
            }
        }
    }
    panic!("could not find binary artifact for {name}");
}

fn start_server(scope: &str) -> (tokio::task::JoinHandle<()>, String) {
    let socket = paths::socket_path(Some(scope));
    let db = paths::db_path(Some(scope)).to_string_lossy().into_owned();
    let server = DaemonServer::new(
        socket.clone(),
        db,
        "pipe-test".to_string(),
        scope.to_string(),
        std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    )
    .expect("DaemonServer::new");
    let handle = tokio::spawn(async move {
        server.run().await.expect("server.run");
    });
    (handle, socket)
}

// (helper intentionally elided: `recv_frame` blocks indefinitely without
// platform-specific socket timeouts, so this test asserts on the initial
// backlog which is delivered inline with the AttachPipeStreamResponse.)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_attach_stdout_then_terminate_lifecycle() {
    let scope = format!("pipe-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let env_reporter = tokio::task::spawn_blocking(|| testbin_path("testbin-env-reporter"))
        .await
        .expect("testbin");

    let socket_for_test = socket.clone();
    tokio::task::spawn_blocking(move || {
        let mut control = DaemonClient::connect_to(&socket_for_test).expect("control connect");
        let argv = vec![env_reporter.to_string_lossy().into_owned()];
        let spawned = control
            .spawn_pipe_session(&PipeSpawnRequest::new(argv).with_originator("pipe-lifecycle-test"))
            .expect("spawn pipe session");
        assert!(!spawned.session_id.is_empty());

        // List shows the new session and neither stream attached.
        let listed = control.list_pipe_sessions("").expect("list");
        let entry = listed
            .iter()
            .find(|s| s.session_id == spawned.session_id)
            .expect("pipe session not present in list");
        assert!(!entry.stdout_attached);
        assert!(!entry.stderr_attached);
        assert!(!entry.exited);

        // Attach to stdout via a separate connection. Give env-reporter
        // a moment to print "PID=…\nORIGINATOR=…\nREADY\n" first so the
        // bytes land in the daemon's ring buffer before our attach.
        std::thread::sleep(Duration::from_millis(500));
        let attachment = PipeStreamAttachment::attach_to(
            &socket_for_test,
            &spawned.session_id,
            PipeStreamKind::Stdout,
            false,
        )
        .expect("attach stdout");

        // initial_backlog is delivered inline with the
        // AttachPipeStreamResponse and should contain READY.
        let text = String::from_utf8_lossy(&attachment.initial_backlog);
        assert!(
            text.contains("READY"),
            "expected READY in initial backlog, got: {text:?}"
        );

        // List should now show stdout_attached=true.
        let listed_after = control
            .list_pipe_sessions("pipe-lifecycle-test")
            .expect("list after attach");
        let entry = listed_after
            .iter()
            .find(|s| s.session_id == spawned.session_id)
            .expect("session disappeared from filtered list");
        assert!(entry.stdout_attached);

        // Concurrent second attach without steal should be rejected.
        match PipeStreamAttachment::attach_to(
            &socket_for_test,
            &spawned.session_id,
            PipeStreamKind::Stdout,
            false,
        ) {
            Ok(_) => panic!("second attach should not succeed without steal"),
            Err(running_process::daemon::pipe_session::PipeAttachError::Server { code, .. }) => {
                assert_eq!(
                    code,
                    running_process::proto::daemon::StatusCode::AlreadyAttached
                );
            }
            Err(other) => panic!("unexpected attach error: {other}"),
        }

        // Drop the attachment to release stdout.
        drop(attachment);
        std::thread::sleep(Duration::from_millis(150));

        // Terminate and wait for exit state.
        control
            .terminate_pipe_session(&spawned.session_id, 1000)
            .expect("terminate");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let listed = control.list_pipe_sessions("").expect("list during wait");
            if let Some(entry) = listed.iter().find(|s| s.session_id == spawned.session_id) {
                if entry.exited {
                    break;
                }
            }
            if Instant::now() >= deadline {
                panic!("pipe session did not exit within budget");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    })
    .await
    .expect("blocking task");
}
