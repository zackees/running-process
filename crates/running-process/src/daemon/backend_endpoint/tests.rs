//! End-to-end tests for the daemon SESSION backend endpoint (soldr#2365): the
//! mux dispatch serving both `BackendHandle` identity probes and `0x5350`
//! SESSION compile sessions on one endpoint. Unix-first.

use futures_util::{SinkExt, StreamExt};
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, ToFsName};

use super::{serve_backend_connection, serve_backend_endpoint};
use crate::broker::backend_handle::{BackendHandle, DaemonProcess};
use crate::broker::protocol::Endpoint;
use crate::broker::protocol_v2::{session_frame, SessionFrame, SessionStart};
use crate::daemon::compile_session::session_framed;

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

fn identity_for(path: &std::path::Path) -> (Endpoint, DaemonProcess) {
    let endpoint = Endpoint {
        namespace_id: "shared".into(),
        path: path.to_string_lossy().into_owned(),
    };
    let identity = DaemonProcess::current_process(endpoint.clone(), Some(30))
        .expect("current-process daemon identity");
    (endpoint, identity)
}

#[cfg(unix)]
#[tokio::test]
async fn mux_backend_endpoint_serves_a_session_compile() {
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("rp-mux-sess-{pid}.sock"));
    let _ = std::fs::remove_file(&path);
    let (_endpoint, identity) = identity_for(&path);

    let listener = ListenerOptions::new()
        .name(
            path.as_path()
                .to_fs_name::<GenericFilePath>()
                .expect("fs name"),
        )
        .create_tokio()
        .expect("bind backend endpoint");
    let daemon = tokio::spawn(serve_backend_endpoint(listener, identity));

    // Client speaks the Model-B SESSION wire directly (the same wire the broker
    // relay carries transparently).
    let stream = interprocess::local_socket::tokio::Stream::connect(
        path.as_path()
            .to_fs_name::<GenericFilePath>()
            .expect("client fs name"),
    )
    .await
    .expect("client dials backend endpoint");
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

    daemon.abort();
    let _ = std::fs::remove_file(&path);

    assert_eq!(
        stdout, b"HELLO",
        "stdout proxied over the mux SESSION endpoint"
    );
    assert_eq!(
        stderr, b"WORLD",
        "stderr proxied over the mux SESSION endpoint"
    );
    assert_eq!(
        code,
        Some(9),
        "exit code proxied over the mux SESSION endpoint"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn mux_backend_endpoint_answers_identity_probe() {
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("rp-mux-probe-{pid}.sock"));
    let _ = std::fs::remove_file(&path);
    let (endpoint, identity) = identity_for(&path);

    let listener = ListenerOptions::new()
        .name(
            path.as_path()
                .to_fs_name::<GenericFilePath>()
                .expect("fs name"),
        )
        .create_tokio()
        .expect("bind backend endpoint");
    let daemon = tokio::spawn(serve_backend_endpoint(listener, identity.clone()));

    // The broker's `BackendHandle::probe_with_service` is the exact registration
    // handshake; it must succeed against the mux endpoint, proving a daemon
    // serving SESSION here still registers as a verified backend.
    let expected = identity.clone();
    let probe = tokio::task::spawn_blocking(move || {
        BackendHandle::probe_with_service("zccache", "1.11.20", &endpoint, &expected)
    })
    .await
    .expect("probe task joined");

    daemon.abort();
    let _ = std::fs::remove_file(&path);

    assert!(
        probe.is_ok(),
        "the mux endpoint must answer the identity probe: {:?}",
        probe.err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn mux_backend_connection_rejects_non_session_first_party_frame() {
    use crate::broker::protocol::{encode_framed, Frame, CONTROL_PAYLOAD_PROTOCOL};

    // A broker Hello (control-plane, first-party) is not valid on a backend
    // endpoint; the mux rejects it rather than mis-serving it as a session.
    let (client, server) = tokio::io::duplex(4096);
    let (_endpoint, identity) = identity_for(std::path::Path::new(
        "/tmp/rp-mux-reject-does-not-bind.sock",
    ));
    let server = tokio::spawn(async move { serve_backend_connection(server, &identity).await });

    let wire = encode_framed(&Frame::request(CONTROL_PAYLOAD_PROTOCOL, Vec::new()))
        .expect("encode control frame");
    use tokio::io::AsyncWriteExt;
    let mut client = client;
    client.write_all(&wire).await.expect("send control frame");
    client.flush().await.expect("flush");

    let result = server.await.expect("server task joined");
    assert!(
        result.is_err(),
        "a first-party control frame on the backend endpoint must be rejected"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn full_vertical_client_broker_relay_daemon_mux_compile() {
    // The whole SESSION vertical over two real sockets: client -> broker relay
    // -> daemon mux endpoint. The broker is transparent (`relay_session` /
    // `copy_bidirectional`); the daemon serves 0x5350 via the mux.
    use crate::broker::session_relay::relay_session;

    let pid = std::process::id();
    let daemon_path = std::env::temp_dir().join(format!("rp-vert-d-{pid}.sock"));
    let broker_path = std::env::temp_dir().join(format!("rp-vert-b-{pid}.sock"));
    let _ = std::fs::remove_file(&daemon_path);
    let _ = std::fs::remove_file(&broker_path);
    let (_endpoint, identity) = identity_for(&daemon_path);

    // Daemon: the real mux SESSION endpoint.
    let daemon_listener = ListenerOptions::new()
        .name(
            daemon_path
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .expect("daemon fs name"),
        )
        .create_tokio()
        .expect("bind daemon endpoint");
    let daemon = tokio::spawn(serve_backend_endpoint(daemon_listener, identity));

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
            args: vec!["out:PROXIED".to_owned(), "exit:3".to_owned()],
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
    let mut code = None;
    while let Some(Ok(sf)) = client.next().await {
        match sf.kind {
            Some(session_frame::Kind::Stdout(b)) => stdout.extend_from_slice(&b),
            Some(session_frame::Kind::Stderr(_)) => {}
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

    assert_eq!(
        stdout, b"PROXIED",
        "stdout proxied client<-broker<-daemon mux"
    );
    assert_eq!(code, Some(3), "exit code proxied across both real sockets");
}
