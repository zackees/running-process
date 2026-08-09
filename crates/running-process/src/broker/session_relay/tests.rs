//! End-to-end test for the broker SESSION relay (soldr#2365): a compile session
//! proxied **client → broker → daemon** across TWO real `interprocess` local
//! sockets, using the production daemon endpoint and the broker's full-proxy
//! relay. The command is carried on the wire; the broker is transparent. This is
//! the production form of the relay vertical (real transport, not the harness
//! `copy_bidirectional` over `UnixStream` pairs). Unix-first.

use futures_util::{SinkExt, StreamExt};
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, ToFsName};

use super::relay_session;
use crate::broker::protocol_v2::{session_frame, SessionFrame, SessionStart};
use crate::daemon::compile_session::session_framed;
use crate::daemon::session_endpoint::serve_session_endpoint;

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
async fn relay_session_proxies_client_to_daemon_endpoint() {
    let pid = std::process::id();
    let daemon_path = std::env::temp_dir().join(format!("rp-relay-d-{pid}.sock"));
    let broker_path = std::env::temp_dir().join(format!("rp-relay-b-{pid}.sock"));
    let _ = std::fs::remove_file(&daemon_path);
    let _ = std::fs::remove_file(&broker_path);

    // Daemon SESSION endpoint (the real serve surface).
    let daemon_listener = ListenerOptions::new()
        .name(
            daemon_path
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .expect("daemon fs name"),
        )
        .create_tokio()
        .expect("bind daemon endpoint");
    let daemon = tokio::spawn(serve_session_endpoint(daemon_listener));

    // Broker: accept the client and full-proxy it to the daemon endpoint.
    let broker_listener = ListenerOptions::new()
        .name(
            broker_path
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .expect("broker fs name"),
        )
        .create_tokio()
        .expect("bind broker endpoint");
    let daemon_path_str = daemon_path.to_string_lossy().into_owned();
    let broker = tokio::spawn(async move {
        let client_conn = broker_listener.accept().await.expect("broker accept");
        let _ = relay_session(client_conn, &daemon_path_str).await;
    });

    // Client dials ONLY the broker and speaks the SESSION wire.
    let stream = interprocess::local_socket::tokio::Stream::connect(
        broker_path
            .as_path()
            .to_fs_name::<GenericFilePath>()
            .expect("client fs name"),
    )
    .await
    .expect("client dials broker");
    let mut client = session_framed(stream);

    client
        .send(frame(session_frame::Kind::Start(SessionStart {
            program: fixture_program(),
            args: vec![
                "out:HELLO".to_owned(),
                "err:WORLD".to_owned(),
                "exit:9".to_owned(),
            ],
            cwd: String::new(),
            env: Vec::new(),
            clear_inherited_env: false,
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

    daemon.abort();
    broker.abort();
    let _ = std::fs::remove_file(&daemon_path);
    let _ = std::fs::remove_file(&broker_path);

    assert_eq!(stdout, b"HELLO", "stdout proxied client←broker←daemon");
    assert_eq!(stderr, b"WORLD", "stderr proxied client←broker←daemon");
    assert_eq!(code, Some(9), "exit code proxied across both real sockets");
}
