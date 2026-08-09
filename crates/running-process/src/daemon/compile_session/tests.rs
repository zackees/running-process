//! Daemon-direct tests for the compile-session handler (soldr#2365, slice 3c):
//! drive [`run_compile_session`] over a tokio `duplex()` wrapped in the daemon's
//! real `Framed<_, LengthDelimitedCodec>` transport — no broker — and assert the
//! proxied bytes match a direct-execution oracle. Run in CI by a scoped
//! `--features daemon -E test(compile_session)` nextest step.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};

use super::{run_compile_session, session_framed};
use crate::broker::protocol_v2::{session_frame, SessionFrame};
use crate::containment::ContainedProcessGroup;

fn fixture() -> Command {
    let exe = std::env::current_exe().expect("test executable path");
    let dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    let path = dir.join(format!(
        "testbin-stdio-scripted{}",
        std::env::consts::EXE_SUFFIX
    ));
    assert!(
        path.is_file(),
        "test fixture is missing at {} — run `soldr cargo build -p testbins` first",
        path.display()
    );
    Command::new(path)
}

fn frame(kind: session_frame::Kind) -> SessionFrame {
    SessionFrame { kind: Some(kind) }
}

#[derive(Debug, PartialEq, Eq)]
struct Reassembled {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: i32,
}

/// Direct execution (the oracle/golden).
fn run_direct(directives: &[&str], stdin: &[u8]) -> Reassembled {
    let mut child = fixture()
        .args(directives)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oracle");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    Reassembled {
        stdout: out.stdout,
        stderr: out.stderr,
        code: out.status.code().unwrap_or(-1),
    }
}

/// Drive the handler over the daemon transport with a codec-speaking client.
async fn run_over_daemon_transport(directives: &[&str], stdin: &[u8]) -> Reassembled {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = session_framed(server_io);
    let mut client = session_framed(client_io);

    let mut cmd = fixture();
    cmd.args(directives);
    let group = Arc::new(ContainedProcessGroup::new().expect("contained group"));

    let handler = tokio::spawn(run_compile_session(server, cmd, group));

    if !stdin.is_empty() {
        client
            .send(frame(session_frame::Kind::Stdin(stdin.to_vec())))
            .await
            .unwrap();
    }
    client
        .send(frame(session_frame::Kind::StdinEof(true)))
        .await
        .unwrap();

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut code = None;
    while let Some(Ok(sf)) = client.next().await {
        match sf.kind {
            Some(session_frame::Kind::Stdout(b)) => stdout.extend_from_slice(&b),
            Some(session_frame::Kind::Stderr(b)) => stderr.extend_from_slice(&b),
            Some(session_frame::Kind::Exit(e)) => {
                code = Some(e.code);
                break;
            }
            _ => panic!("unexpected inbound-only frame on the outbound lane"),
        }
    }

    let exit = handler.await.unwrap().expect("handler reaps child");
    assert_eq!(code, Some(exit.code), "client-visible exit matches handler");
    Reassembled {
        stdout,
        stderr,
        code: exit.code,
    }
}

#[tokio::test]
async fn compile_session_matches_oracle_over_daemon_transport() {
    let script = &["out:ARTIFACT", "err:DIAGNOSTIC", "out:MORE", "exit:5"];
    assert_eq!(
        run_over_daemon_transport(script, b"").await,
        run_direct(script, b"")
    );
}

#[tokio::test]
async fn compile_session_is_byte_transparent_for_raw_non_utf8() {
    let script = &["outhex:00ff80", "errhex:8081ff"];
    let got = run_over_daemon_transport(script, b"").await;
    assert_eq!(got.stdout, vec![0x00, 0xff, 0x80]);
    assert_eq!(got.stderr, vec![0x80, 0x81, 0xff]);
    assert_eq!(got, run_direct(script, b""));
}

#[tokio::test]
async fn compile_session_delivers_full_byte_range_stdin() {
    let payload: Vec<u8> = (0u8..=255).collect();
    let got = run_over_daemon_transport(&["echo"], &payload).await;
    assert_eq!(got.stdout, payload);
    assert_eq!(got, run_direct(&["echo"], &payload));
}
