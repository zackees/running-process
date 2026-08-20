//! End-to-end test for the daemon SESSION backend endpoint (soldr#2365): over a
//! real `interprocess` local socket, the endpoint accepts a connection, reads
//! the client's `SessionStart`, runs the contained compile, and proxies its
//! stdio back. The command is carried on the wire; this crosses a real IPC
//! boundary through the production serve surface. Unix-first.

use futures_util::{SinkExt, StreamExt};

use super::serve_session_endpoint;
use crate::broker::protocol_v2::{session_frame, SessionFrame, SessionStart};
use crate::daemon::compile_session::session_framed;
use crate::platform::ipc::{AsyncListener, AsyncStream, Endpoint};

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

fn frame(kind: session_frame::Kind) -> SessionFrame {
    SessionFrame { kind: Some(kind) }
}

#[cfg(unix)]
#[tokio::test]
async fn session_endpoint_serves_a_session_from_start_frame() {
    let path = std::env::temp_dir().join(format!("rp-endpoint-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let ipc_endpoint = Endpoint::new(path.to_string_lossy().into_owned()).expect("IPC endpoint");
    let listener = AsyncListener::bind(&ipc_endpoint).expect("bind session endpoint");

    let endpoint = tokio::spawn(serve_session_endpoint(listener));

    // Client dials the real endpoint and speaks the SESSION wire.
    let stream = AsyncStream::connect(&ipc_endpoint)
        .await
        .expect("connect session endpoint");
    let mut client = session_framed(stream);

    client
        .send(frame(session_frame::Kind::Start(SessionStart {
            program: fixture_program(),
            args: vec![
                "out:HELLO".to_owned(),
                "err:WORLD".to_owned(),
                "exit:7".to_owned(),
            ],
            cwd: String::new(),
            env: Vec::new(),
            clear_inherited_env: false,
            environment_policy: 0,
        })))
        .await
        .expect("send start");
    client
        .send(frame(session_frame::Kind::StdinEof(true)))
        .await
        .expect("send stdin eof");

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

    endpoint.abort();
    let _ = std::fs::remove_file(&path);

    assert_eq!(stdout, b"HELLO", "stdout from the wire-carried command");
    assert_eq!(stderr, b"WORLD", "stderr from the wire-carried command");
    assert_eq!(code, Some(7), "exit code proxied over the real endpoint");
}
