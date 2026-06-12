#![cfg(feature = "daemon")]
//! Integration test for `GetSessionBacklog` / `sessions log`
//! (#130 milestone 7 B4).
//!
//! Asserts the daemon can snapshot a session's captured output without
//! consuming it: two back-to-back snapshots see the same bytes, and a
//! subsequent attach (which DOES drain) still receives the same backlog.

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
        .args([
            "build",
            "-p",
            "testbins",
            "--bin",
            name,
            "--message-format=json",
        ])
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
        "sessions-log-test".to_string(),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_does_not_consume_backlog_for_pipe_sessions() {
    let scope = format!("snapshot-pipe-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let env_reporter = tokio::task::spawn_blocking(|| testbin_path("testbin-env-reporter"))
        .await
        .expect("testbin");
    let socket_for_test = socket.clone();

    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");
        let session = client
            .spawn_pipe_session(
                &PipeSpawnRequest::new([env_reporter.to_string_lossy().into_owned()])
                    .with_originator("snapshot-test"),
            )
            .expect("spawn");

        // Give env-reporter time to print PID=, ORIGINATOR=, READY.
        std::thread::sleep(Duration::from_millis(500));

        // First snapshot: backlog should contain READY.
        let snap1 = client
            .get_session_backlog(&session.session_id, PipeStreamKind::Stdout)
            .expect("snapshot 1")
            .expect("session present");
        assert_eq!(snap1.session_kind, "pipe");
        assert!(!snap1.exited, "child should still be running");
        let text1 = String::from_utf8_lossy(&snap1.backlog).into_owned();
        assert!(
            text1.contains("READY"),
            "first snapshot should include READY, got: {text1:?}"
        );

        // Second snapshot: same bytes still present (no draining).
        let snap2 = client
            .get_session_backlog(&session.session_id, PipeStreamKind::Stdout)
            .expect("snapshot 2")
            .expect("session present");
        let text2 = String::from_utf8_lossy(&snap2.backlog).into_owned();
        assert!(
            text2.contains("READY"),
            "second snapshot should still include READY (no consume), got: {text2:?}"
        );

        // Attach (which DOES drain) — initial_backlog should ALSO contain
        // READY, proving the snapshot did not consume it for the attach
        // path.
        let attachment = PipeStreamAttachment::attach_to(
            &socket_for_test,
            &session.session_id,
            PipeStreamKind::Stdout,
            false,
        )
        .expect("attach");
        let attach_text = String::from_utf8_lossy(&attachment.initial_backlog).into_owned();
        assert!(
            attach_text.contains("READY"),
            "attach initial_backlog should still see READY after two snapshots, got: {attach_text:?}"
        );
        drop(attachment);

        // Unknown session id → NotFound surfaces as Ok(None).
        let missing = client
            .get_session_backlog("does-not-exist", PipeStreamKind::Stdout)
            .expect("snapshot for missing id");
        assert!(missing.is_none());

        // Cleanup.
        client
            .terminate_pipe_session(&session.session_id, 500)
            .expect("terminate");
    })
    .await
    .expect("blocking task");
}
