//! Tests for the dumb-terminal SESSION client (soldr#2365 slice 4): drive
//! [`run_session_client`] against the real [`serve_session`] handler over an
//! in-memory duplex and assert the client renders the compile's stdout/stderr
//! and returns its exit code — the full client↔daemon SESSION round-trip.

use std::sync::{Arc, Mutex};

use super::run_session_client;
use crate::broker::protocol_v2::SessionStart;
use crate::containment::ContainedProcessGroup;
use crate::daemon::compile_session::{serve_session, session_framed};

fn fixture_program() -> String {
    let exe = std::env::current_exe().expect("test executable path");
    let dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    dir.join(format!(
        "testbin-stdio-scripted{}",
        std::env::consts::EXE_SUFFIX
    ))
    .to_string_lossy()
    .into_owned()
}

/// An `AsyncWrite` that captures everything written, so the test can inspect what
/// the dumb terminal rendered to "stdout"/"stderr".
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }
}

impl tokio::io::AsyncWrite for Capture {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.0.lock().unwrap().extend_from_slice(buf);
        std::task::Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn dumb_terminal_client_renders_output_and_returns_exit_code() {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let group = Arc::new(ContainedProcessGroup::new().expect("contained group"));
    let server = tokio::spawn(serve_session(session_framed(server_io), group));

    let stdout = Capture::default();
    let stderr = Capture::default();
    let start = SessionStart {
        program: fixture_program(),
        args: vec![
            "out:ARTIFACT".to_owned(),
            "err:DIAGNOSTIC".to_owned(),
            "out:MORE".to_owned(),
            "exit:5".to_owned(),
        ],
        cwd: String::new(),
        env: Vec::new(),
        clear_inherited_env: false,
        environment_policy: 0,
    };

    // Empty stdin: the client sends StdinEof immediately (rustc doesn't read it).
    let code = run_session_client(
        session_framed(client_io),
        start,
        tokio::io::empty(),
        stdout.clone(),
        stderr.clone(),
    )
    .await
    .expect("client drives the session");

    let _ = server.await.expect("server task");

    assert_eq!(stdout.bytes(), b"ARTIFACTMORE", "client rendered stdout");
    assert_eq!(stderr.bytes(), b"DIAGNOSTIC", "client rendered stderr");
    assert_eq!(code, 5, "client returned the session exit code");
}

#[tokio::test]
async fn dumb_terminal_client_forwards_stdin() {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let group = Arc::new(ContainedProcessGroup::new().expect("contained group"));
    let server = tokio::spawn(serve_session(session_framed(server_io), group));

    let stdout = Capture::default();
    let stderr = Capture::default();
    let start = SessionStart {
        program: fixture_program(),
        args: vec!["echo".to_owned()],
        cwd: String::new(),
        env: Vec::new(),
        clear_inherited_env: false,
        environment_policy: 0,
    };

    // `echo` reads stdin to EOF and writes it back; the client must forward the
    // provided stdin and render the echoed bytes. Feed the payload through a
    // duplex whose write half is dropped to signal EOF.
    let payload: Vec<u8> = (0u8..=255).collect();
    let (mut feed, stdin_read) = tokio::io::duplex(1024);
    let payload_for_feed = payload.clone();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let _ = feed.write_all(&payload_for_feed).await;
        // `feed` drops here → the client's stdin reaches EOF.
    });

    let code = run_session_client(
        session_framed(client_io),
        start,
        stdin_read,
        stdout.clone(),
        stderr.clone(),
    )
    .await
    .expect("client drives the session");

    let _ = server.await.expect("server task");
    assert_eq!(
        stdout.bytes(),
        payload,
        "client forwarded stdin, echoed back"
    );
    assert_eq!(code, 0);
}
