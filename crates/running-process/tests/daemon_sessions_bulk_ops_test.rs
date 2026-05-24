#![cfg(feature = "daemon")]
//! Bulk session ops (#130 M9 H4 follow-up).
//!
//! Verifies `purge_exited_sessions` removes exited sessions but leaves
//! live ones, and `bulk_terminate_sessions` schedules termination for
//! sessions older than a threshold while leaving newer ones running.

use running_process::daemon::client::DaemonClient;
use running_process::daemon::paths;
use running_process::daemon::pipe_session::PipeSpawnRequest;
use running_process::daemon::server::DaemonServer;

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
        "bulk-ops-test".to_string(),
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
async fn purge_removes_only_exited_sessions() {
    let scope = format!("bulk-purge-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let env_reporter = tokio::task::spawn_blocking(|| testbin_path("testbin-env-reporter"))
        .await
        .expect("testbin");
    let socket_for_test = socket.clone();

    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");

        // Spawn two pipe sessions.
        let alive = client
            .spawn_pipe_session(
                &PipeSpawnRequest::new([env_reporter.to_string_lossy().into_owned()])
                    .with_originator("bulk-purge"),
            )
            .expect("spawn alive");
        let to_terminate = client
            .spawn_pipe_session(
                &PipeSpawnRequest::new([env_reporter.to_string_lossy().into_owned()])
                    .with_originator("bulk-purge"),
            )
            .expect("spawn to_terminate");

        // Terminate one of them and wait for it to actually exit.
        client
            .terminate_pipe_session(&to_terminate.session_id, 500)
            .expect("terminate");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let listed = client.list_pipe_sessions("bulk-purge").expect("list");
            if let Some(e) = listed
                .iter()
                .find(|s| s.session_id == to_terminate.session_id)
            {
                if e.exited {
                    break;
                }
            }
            if Instant::now() >= deadline {
                panic!("session did not exit");
            }
            std::thread::sleep(Duration::from_millis(200));
        }

        // Purge should remove the exited session but leave the alive one.
        let purged = client
            .purge_exited_sessions("bulk-purge")
            .expect("purge");
        assert_eq!(purged.pty_purged, 0);
        assert_eq!(purged.pipe_purged, 1);

        // List confirms only the alive session remains.
        let remaining = client.list_pipe_sessions("bulk-purge").expect("list after");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].session_id, alive.session_id);

        // Cleanup.
        client
            .terminate_pipe_session(&alive.session_id, 500)
            .expect("terminate cleanup");
    })
    .await
    .expect("blocking task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bulk_terminate_older_than_zero_terminates_everything_in_scope() {
    let scope = format!("bulk-kill-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let env_reporter = tokio::task::spawn_blocking(|| testbin_path("testbin-env-reporter"))
        .await
        .expect("testbin");
    let socket_for_test = socket.clone();

    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");

        // Spawn 3 sessions in this scope's originator.
        let mut ids = Vec::new();
        for _ in 0..3 {
            let session = client
                .spawn_pipe_session(
                    &PipeSpawnRequest::new([env_reporter.to_string_lossy().into_owned()])
                        .with_originator("bulk-kill"),
                )
                .expect("spawn");
            ids.push(session.session_id);
        }
        // And one with a different originator to confirm filtering.
        let untouched = client
            .spawn_pipe_session(
                &PipeSpawnRequest::new([env_reporter.to_string_lossy().into_owned()])
                    .with_originator("other"),
            )
            .expect("spawn untouched");
        ids.push(untouched.session_id.clone());

        // older_than=0 + originator="bulk-kill" terminates the 3 matching.
        let result = client
            .bulk_terminate_sessions(0, "bulk-kill", 500)
            .expect("bulk terminate");
        assert_eq!(result.pty_terminated, 0);
        assert_eq!(result.pipe_terminated, 3);

        // Wait for them to actually exit.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let listed = client.list_pipe_sessions("bulk-kill").expect("list");
            if listed.iter().all(|s| s.exited) {
                break;
            }
            if Instant::now() >= deadline {
                panic!("bulk-killed sessions did not exit");
            }
            std::thread::sleep(Duration::from_millis(200));
        }

        // The untouched session should still be alive.
        let other = client.list_pipe_sessions("other").expect("list other");
        let untouched_entry = other
            .iter()
            .find(|s| s.session_id == untouched.session_id)
            .expect("untouched session present");
        assert!(
            !untouched_entry.exited,
            "untouched (different originator) must still be alive"
        );

        // Cleanup.
        client
            .terminate_pipe_session(&untouched.session_id, 500)
            .expect("terminate untouched");
    })
    .await
    .expect("blocking task");
}
