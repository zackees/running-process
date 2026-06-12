#![cfg(feature = "daemon")]
//! Backlog accumulation while detached (#130 milestone 5 C2).
//!
//! Spawn a child that emits continuous output (`testbin-emitter` prints
//! "tick N" every ~50 ms). Snapshot the backlog twice with a delay,
//! assert it grew. Then attach and assert the attach receives a
//! non-empty initial_backlog that contains a "tick" line. This exercises
//! the C1 / C2 invariants from the meta-issue: ring buffer fills while
//! detached, replays on attach.

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
        "backlog-accum-test".to_string(),
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
async fn backlog_accumulates_while_no_client_attached_and_replays_on_attach() {
    let scope = format!("backlog-accum-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let emitter = tokio::task::spawn_blocking(|| testbin_path("testbin-emitter"))
        .await
        .expect("testbin");
    let socket_for_test = socket.clone();

    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");
        let session = client
            .spawn_pipe_session(
                &PipeSpawnRequest::new([emitter.to_string_lossy().into_owned()])
                    .with_originator("backlog-accum"),
            )
            .expect("spawn");

        // Let the emitter run for a bit while NOT attached.
        std::thread::sleep(Duration::from_millis(500));

        // First snapshot.
        let snap1 = client
            .get_session_backlog(&session.session_id, PipeStreamKind::Stdout)
            .expect("snap1")
            .expect("session present");
        let text1 = String::from_utf8_lossy(&snap1.backlog).into_owned();
        let len1 = snap1.backlog.len();
        assert!(
            text1.contains("tick"),
            "first snapshot should already contain tick lines, got len={} text_preview={:?}",
            len1,
            &text1.chars().take(80).collect::<String>()
        );

        // Wait again; backlog should grow because the emitter keeps printing
        // and no one is draining it.
        std::thread::sleep(Duration::from_millis(800));
        let snap2 = client
            .get_session_backlog(&session.session_id, PipeStreamKind::Stdout)
            .expect("snap2")
            .expect("session present");
        let len2 = snap2.backlog.len();
        assert!(
            len2 > len1,
            "backlog should grow while no client is attached: len1={len1} len2={len2}"
        );

        // Attach: initial_backlog drains the ring buffer. The drained
        // bytes should include at least a "tick" line.
        let attachment = PipeStreamAttachment::attach_to(
            &socket_for_test,
            &session.session_id,
            PipeStreamKind::Stdout,
            false,
        )
        .expect("attach");
        let attach_text = String::from_utf8_lossy(&attachment.initial_backlog).into_owned();
        assert!(
            attach_text.contains("tick"),
            "attach initial_backlog should contain tick lines, got len={} preview={:?}",
            attachment.initial_backlog.len(),
            &attach_text.chars().take(80).collect::<String>()
        );

        // After attach drains the buffer, a fresh snapshot should be smaller
        // than the just-drained backlog (the ring buffer was emptied by
        // attach + may have a few new ticks since).
        let snap3 = client
            .get_session_backlog(&session.session_id, PipeStreamKind::Stdout)
            .expect("snap3")
            .expect("session present");
        assert!(
            snap3.backlog.len() < attachment.initial_backlog.len(),
            "snapshot after attach drain should be smaller than the drained backlog:\
             post={} drained={}",
            snap3.backlog.len(),
            attachment.initial_backlog.len()
        );

        drop(attachment);
        client
            .terminate_pipe_session(&session.session_id, 500)
            .expect("terminate");
    })
    .await
    .expect("blocking task");
}
