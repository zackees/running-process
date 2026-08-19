//! End-to-end test for the broker SESSION relay (soldr#2365): a compile session
//! proxied **client → broker → daemon** across TWO real `interprocess` local
//! sockets, using the production daemon endpoint and the broker's full-proxy
//! relay. The command is carried on the wire; the broker is transparent. This is
//! the production form of the relay vertical (real transport, including Linux
//! splice rather than an in-memory `UnixStream` approximation). Unix-first.

use futures_util::{SinkExt, StreamExt};
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, ToFsName};

use super::relay_local_socket_session;
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

#[tokio::test]
async fn relay_session_proxies_client_to_daemon_endpoint() {
    if std::env::consts::OS == "windows" {
        // This transport oracle uses filesystem local-socket names. Windows
        // exercises the same relay through its named-pipe integration tests.
        return;
    }
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
        let _ = relay_local_socket_session(client_conn, &daemon_path_str).await;
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
    broker.abort();
    let _ = std::fs::remove_file(&daemon_path);
    let _ = std::fs::remove_file(&broker_path);

    assert_eq!(stdout, b"HELLO", "stdout proxied client←broker←daemon");
    assert_eq!(stderr, b"WORLD", "stderr proxied client←broker←daemon");
    assert_eq!(code, Some(9), "exit code proxied across both real sockets");
}

/// The opaque `SessionExit.metadata` map (soldr#2365 Q3) must cross the relay
/// untouched — `relay_session` never decodes the frame, so a consumer daemon's
/// `cache_outcome` / `compile_id` reach the client verbatim. The opening
/// `SessionStart` is equally opaque: its environment policy and ordered,
/// case-distinct environment entries must reach the daemon unchanged.
#[cfg(unix)]
#[tokio::test]
async fn relay_session_preserves_start_environment_and_exit_metadata() {
    use crate::broker::protocol_v2::SessionExit;

    let pid = std::process::id();
    let daemon_path = std::env::temp_dir().join(format!("rp-relay-md-d-{pid}.sock"));
    let broker_path = std::env::temp_dir().join(format!("rp-relay-md-b-{pid}.sock"));
    let _ = std::fs::remove_file(&daemon_path);
    let _ = std::fs::remove_file(&broker_path);

    // Stub daemon: accept, read the client's opening frame, reply with one Exit
    // carrying a metadata map, then close.
    let daemon_listener = ListenerOptions::new()
        .name(
            daemon_path
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .expect("daemon fs name"),
        )
        .create_tokio()
        .expect("bind daemon endpoint");
    let daemon = tokio::spawn(async move {
        let conn = daemon_listener.accept().await.expect("daemon accept");
        let mut d = session_framed(conn);
        let start = d
            .next()
            .await
            .expect("client sent an opening frame")
            .expect("opening frame decodes");
        let _ = d
            .send(frame(session_frame::Kind::Exit(SessionExit {
                code: 0,
                signal: 0,
                metadata: [("cache_outcome".to_owned(), "hit".to_owned())]
                    .into_iter()
                    .collect(),
            })))
            .await;
        start
    });

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
        let _ = relay_local_socket_session(client_conn, &daemon_path_str).await;
    });

    let stream = interprocess::local_socket::tokio::Stream::connect(
        broker_path
            .as_path()
            .to_fs_name::<GenericFilePath>()
            .expect("client fs name"),
    )
    .await
    .expect("client dials broker");
    let mut client = session_framed(stream);
    let expected = SessionStart {
        program: "env-probe".to_owned(),
        args: vec!["--ordered".to_owned()],
        cwd: "/client/work".to_owned(),
        env: vec![
            crate::broker::protocol_v2::SessionEnvVar {
                key: "Path".to_owned(),
                value: "first".to_owned(),
            },
            crate::broker::protocol_v2::SessionEnvVar {
                key: "PATH".to_owned(),
                value: "second".to_owned(),
            },
        ],
        clear_inherited_env: true,
        environment_policy: 3,
    };
    client
        .send(frame(session_frame::Kind::Start(expected.clone())))
        .await
        .expect("send start");

    let mut got = None;
    while let Some(Ok(sf)) = client.next().await {
        if let Some(session_frame::Kind::Exit(e)) = sf.kind {
            got = Some(e);
            break;
        }
    }

    let relayed = tokio::time::timeout(std::time::Duration::from_secs(5), daemon)
        .await
        .expect("daemon receives opening frame")
        .expect("daemon task");
    broker.abort();
    let _ = std::fs::remove_file(&daemon_path);
    let _ = std::fs::remove_file(&broker_path);

    let exit = got.expect("received an Exit frame through the relay");
    assert_eq!(
        relayed.kind,
        Some(session_frame::Kind::Start(expected)),
        "broker must preserve the policy field and ordered environment entries"
    );
    assert_eq!(
        exit.metadata.get("cache_outcome").map(String::as_str),
        Some("hit"),
        "SessionExit.metadata must survive relay_session uninterpreted"
    );
}

/// Cancelling the long-lived production relay must close every original and
/// duplicated descriptor so neither peer waits forever on an orphaned pipe.
#[tokio::test]
async fn relay_session_cancellation_closes_both_peers() {
    if std::env::consts::OS != "linux" {
        return;
    }
    use tokio::io::AsyncReadExt;

    let pid = std::process::id();
    let daemon_path = std::env::temp_dir().join(format!("rp-relay-cancel-d-{pid}.sock"));
    let broker_path = std::env::temp_dir().join(format!("rp-relay-cancel-b-{pid}.sock"));
    let _ = std::fs::remove_file(&daemon_path);
    let _ = std::fs::remove_file(&broker_path);

    let daemon_listener = ListenerOptions::new()
        .name(
            daemon_path
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .expect("daemon fs name"),
        )
        .create_tokio()
        .expect("bind daemon endpoint");
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let daemon = tokio::spawn(async move {
        let mut conn = daemon_listener.accept().await.expect("daemon accept");
        let _ = accepted_tx.send(());
        let mut byte = [0_u8; 1];
        tokio::time::timeout(std::time::Duration::from_secs(2), conn.read(&mut byte))
            .await
            .expect("daemon observes relay cancellation")
            .expect("daemon read")
    });

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
        let client = broker_listener.accept().await.expect("broker accept");
        relay_local_socket_session(client, &daemon_path_str).await
    });

    let mut client = interprocess::local_socket::tokio::Stream::connect(
        broker_path
            .as_path()
            .to_fs_name::<GenericFilePath>()
            .expect("client fs name"),
    )
    .await
    .expect("client dials broker");
    accepted_rx.await.expect("relay connected to daemon");
    broker.abort();
    let _ = broker.await;

    let mut byte = [0_u8; 1];
    let client_read =
        tokio::time::timeout(std::time::Duration::from_secs(2), client.read(&mut byte))
            .await
            .expect("client observes relay cancellation")
            .expect("client read");
    let daemon_read = daemon.await.expect("daemon task");

    let _ = std::fs::remove_file(&daemon_path);
    let _ = std::fs::remove_file(&broker_path);
    assert_eq!(client_read, 0, "client must observe broker-side EOF");
    assert_eq!(daemon_read, 0, "daemon must observe broker-side EOF");
}
