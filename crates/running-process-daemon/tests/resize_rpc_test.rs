//! Resize PTY session while detached (#130 M5 follow-up).

use running_process_daemon::client::DaemonClient;
use running_process_daemon::paths;
use running_process_daemon::pty_session::PtySpawnRequest;
use running_process_daemon::server::DaemonServer;

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

fn testbin_path(name: &str) -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .args(["build", "-p", name, "--message-format=json"])
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
        "resize-rpc-test".to_string(),
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
async fn resize_pty_session_without_attach_updates_rows_cols() {
    let scope = format!("resize-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let sleeper = tokio::task::spawn_blocking(|| testbin_path("testbin-sleeper"))
        .await
        .expect("testbin");
    let socket_for_test = socket.clone();

    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");

        // Spawn with default 24x80.
        let session = client
            .spawn_pty_session(
                &PtySpawnRequest::new([sleeper.to_string_lossy().into_owned()])
                    .with_originator("resize-rpc")
                    .with_size(24, 80),
            )
            .expect("spawn");

        let listed = client.list_pty_sessions("").expect("list");
        let entry = listed
            .iter()
            .find(|s| s.session_id == session.session_id)
            .expect("session");
        assert_eq!(entry.rows, 24);
        assert_eq!(entry.cols, 80);

        // Resize while no client is attached.
        client
            .resize_pty_session(&session.session_id, 50, 120)
            .expect("resize");

        let listed = client.list_pty_sessions("").expect("list after resize");
        let entry = listed
            .iter()
            .find(|s| s.session_id == session.session_id)
            .expect("session");
        assert_eq!(entry.rows, 50, "rows should reflect the RPC resize");
        assert_eq!(entry.cols, 120, "cols should reflect the RPC resize");

        // Unknown session id returns NotFound.
        let err = client
            .resize_pty_session("does-not-exist", 10, 10)
            .expect_err("unknown id");
        match err {
            running_process_daemon::client::ClientError::Server { code, .. } => {
                assert_eq!(code, running_process_proto::daemon::StatusCode::NotFound);
            }
            other => panic!("unexpected error: {other}"),
        }

        client
            .terminate_pty_session(&session.session_id, 500)
            .expect("terminate");
    })
    .await
    .expect("blocking task");
}
