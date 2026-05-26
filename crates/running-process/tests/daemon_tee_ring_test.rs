#![cfg(feature = "daemon")]
//! First #131 telemetry slice: daemon-owned sessions can tee stream bytes into
//! non-blocking in-memory rings while no client is attached.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use running_process::daemon::pipe_sessions::{PipeSessionRegistry, PipeStreamSelect};
#[cfg(not(windows))]
use running_process::daemon::pty_sessions::PtySessionRegistry;
use running_process::daemon::telemetry::TeeStream;

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

fn wait_until_contains<F>(mut read: F, needle: &[u8]) -> Vec<u8>
where
    F: FnMut() -> Vec<u8>,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let bytes = read();
        if bytes.windows(needle.len()).any(|window| window == needle) {
            return bytes;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for {:?} in {:?}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&bytes)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_exit<F>(mut exited: F)
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    while !exited() {
        if Instant::now() >= deadline {
            panic!("session did not exit within deadline");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn pipe_stdout_tee_ring_accumulates_without_attachment() {
    let emitter = testbin_path("testbin-emitter");
    let registry = Arc::new(PipeSessionRegistry::new());
    let session = registry
        .spawn(
            vec![emitter.to_string_lossy().into_owned()],
            None,
            None,
            "tee-ring-test".to_string(),
            "testbin-emitter".to_string(),
            false,
        )
        .expect("spawn pipe session");

    let handle = session
        .tee_stream_ring(PipeStreamSelect::Stdout, 128)
        .expect("register stdout tee");

    let bytes = wait_until_contains(
        || session.tee_snapshot(handle).expect("snapshot").bytes,
        b"tick",
    );
    let snapshot = session.tee_snapshot(handle).expect("snapshot");
    assert_eq!(snapshot.stream, TeeStream::Stdout);
    assert_eq!(snapshot.bytes, bytes);
    assert_eq!(snapshot.capacity, 128);

    let _ = session.terminate(Duration::from_millis(100));
    wait_for_exit(|| session.exit_state().is_some());
    registry.remove(&session.id);
}

#[test]
#[cfg(not(windows))]
fn pty_output_tee_ring_accumulates_without_attachment() {
    let emitter = testbin_path("testbin-emitter");
    let registry = Arc::new(PtySessionRegistry::new());
    let session = registry
        .spawn(
            vec![emitter.to_string_lossy().into_owned()],
            None,
            None,
            24,
            80,
            "tee-ring-test".to_string(),
            "testbin-emitter".to_string(),
        )
        .expect("spawn pty session");

    let handle = session.tee_output_ring(128);

    let bytes = wait_until_contains(
        || session.tee_snapshot(handle).expect("snapshot").bytes,
        b"tick",
    );
    let snapshot = session.tee_snapshot(handle).expect("snapshot");
    assert_eq!(snapshot.stream, TeeStream::PtyOutput);
    assert_eq!(snapshot.bytes, bytes);
    assert_eq!(snapshot.capacity, 128);

    let _ = session.terminate(Duration::from_millis(100));
    wait_for_exit(|| session.exit_state().is_some());
    registry.remove(&session.id);
}
