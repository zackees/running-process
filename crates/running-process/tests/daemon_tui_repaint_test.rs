#![cfg(feature = "daemon")]
//! End-to-end PTY passthrough test for #150.
//!
//! Spawns `testbin-tui-counter` (writes `\x1b[H\x1b[2J` clear, then
//! 10 ticks of `\x1b[1;1HCOUNTER: N\r\n`) as a daemon PTY session,
//! waits for it to finish, then attaches and asserts the initial
//! backlog contains the raw ANSI bytes the child emitted — proving
//! `PSEUDOCONSOLE_PASSTHROUGH_MODE` is active on Windows and that
//! POSIX PTY semantics still hold on Unix.
//!
//! Pre-#150 this test would fail on Windows because portable-pty
//! 0.9.0 doesn't expose the PASSTHROUGH flag and ConPTY would
//! synthesize a virtual-screen re-emission instead of forwarding the
//! child's bytes verbatim. With the W3-W5 ConPTY rewrite the bytes
//! flow through unmodified, so the assertions below pass on all
//! platforms (Windows, macOS, Linux).

use running_process::daemon::client::DaemonClient;
use running_process::daemon::paths;
use running_process::daemon::pty_session::{PtyAttachment, PtySpawnRequest};
use running_process::daemon::server::DaemonServer;
use running_process::proto::daemon::pty_stream_frame::Frame as StreamOneof;

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

fn testbin_path(name: &str) -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .args(["build", "-p", "testbins", "--bin", name, "--message-format=json"])
        .stderr(std::process::Stdio::inherit())
        .output()
        .expect("cargo build for testbin failed");
    assert!(output.status.success(), "cargo build -p {name} failed");

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
        "tui-repaint-test".to_string(),
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

/// Drain the attachment, concatenating every data frame into a single
/// Vec<u8> for byte-exact assertions. Stops when the deadline passes
/// or a frame other than `Data` arrives (e.g. SessionExited).
fn drain_attachment(att: &mut PtyAttachment, deadline: Instant) -> Vec<u8> {
    let mut out = att.initial_backlog.clone();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match att.recv_frame_with_timeout(remaining) {
            Ok(Some(frame)) => match frame.frame {
                Some(StreamOneof::Output(bytes)) => out.extend_from_slice(&bytes),
                // ExitCode / Error / MissedBytes / etc. — stop draining.
                Some(_) => break,
                None => continue,
            },
            Ok(None) => break,
            Err(_) => break,
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_ansi_bytes_flow_through_pty_to_ring_buffer() {
    let scope = format!("tui-repaint-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let tui_counter = tokio::task::spawn_blocking(|| testbin_path("testbin-tui-counter"))
        .await
        .expect("testbin");
    let socket_for_test = socket.clone();

    tokio::task::spawn_blocking(move || {
        // ── Spawn the TUI counter via the daemon ────────────────────────
        let mut control = DaemonClient::connect_to(&socket_for_test).expect("connect");
        let argv = vec![tui_counter.to_string_lossy().into_owned()];
        let spawn_req = PtySpawnRequest::new(argv)
            .with_size(24, 80)
            .with_originator("tui-repaint-test");
        let spawned = control
            .spawn_pty_session(&spawn_req)
            .expect("spawn_pty_session");
        assert!(spawned.pid > 0);

        // ── Let the testbin finish (~500ms of ticks + headroom) ─────────
        std::thread::sleep(Duration::from_millis(900));

        // ── Attach, snapshot the initial backlog ────────────────────────
        let mut att =
            PtyAttachment::attach_to(&socket_for_test, &spawned.session_id, 30, 100, false)
                .expect("attach");
        let deadline = Instant::now() + Duration::from_millis(500);
        let bytes = drain_attachment(&mut att, deadline);
        assert!(
            !bytes.is_empty(),
            "expected non-empty backlog after testbin ran for 500ms"
        );

        // ── Byte-exact ANSI assertions (PASSTHROUGH proof) ──────────────
        // The clear sequence must appear verbatim.
        assert!(
            bytes.windows(4).any(|w| w == b"\x1b[2J"),
            "clear-screen escape `\\x1b[2J` missing from backlog: {:?}",
            String::from_utf8_lossy(&bytes)
        );
        // The cursor-home escape must appear verbatim too.
        assert!(
            bytes.windows(6).any(|w| w == b"\x1b[1;1H"),
            "cursor-home escape `\\x1b[1;1H` missing from backlog: {:?}",
            String::from_utf8_lossy(&bytes)
        );

        // Plaintext counter values must appear (proves the ASCII path).
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(
            text.contains("COUNTER: 0"),
            "first counter line missing from backlog: {text}"
        );
        assert!(
            text.contains("COUNTER: 9"),
            "last counter line missing from backlog: {text}"
        );

        let _ = control.shutdown(true, 5.0);
    })
    .await
    .expect("client task");
}
