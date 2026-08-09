//! End-to-end test for the async broker SESSION serve path (soldr#2365): a
//! compile session proxied **client → broker → daemon**, where the broker now
//! runs a real async Hello round-trip (reusing the sync `HelloResponder`)
//! BEFORE upgrading the same connection to a full-proxy relay. This is the
//! async twin of `session_relay::tests`, with the Hello negotiation added in
//! front. Unix-first.

use futures_util::{SinkExt, StreamExt};
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, ToFsName};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{serve_broker_session_endpoint, ENVELOPE_VERSION};
use crate::broker::protocol::{
    encode_framed, hello_reply::Result as HelloReplyResult, Frame, HelloReply, Negotiated,
    CONTROL_PAYLOAD_PROTOCOL,
};
use crate::broker::protocol_v2::{session_frame, SessionFrame, SessionStart};
use crate::broker::server::connection::{HelloResponder, PeerCredentialPolicy};
use crate::broker::server::hello_handler::PeerIdentity;
use crate::daemon::compile_session::session_framed;
use crate::daemon::session_endpoint::serve_session_endpoint;

/// A permissive responder that negotiates every Hello, pointing the relay at a
/// fixed `backend_pipe` — isolates the async transport under test from the
/// (separately-tested) routing policy while still exercising the production
/// per-connection `Negotiated.backend_pipe` relay-target resolution.
struct NegotiateToBackend(String);

impl HelloResponder for NegotiateToBackend {
    fn handle_frame(&self, _frame: Frame, _peer: PeerIdentity) -> HelloReply {
        HelloReply {
            result: Some(HelloReplyResult::Negotiated(Negotiated {
                backend_pipe: self.0.clone(),
                ..Default::default()
            })),
        }
    }
}

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

/// Read exactly one `[1][u32 LE len][body]` frame off the client stream — the
/// symmetric client-side counterpart to the broker's manual Hello read, so no
/// SESSION bytes are consumed early.
async fn read_framed_body<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> Vec<u8> {
    let mut version = [0u8; 1];
    stream.read_exact(&mut version).await.expect("read version");
    assert_eq!(version[0], ENVELOPE_VERSION, "framing version");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.expect("read len");
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.expect("read body");
    body
}

#[cfg(unix)]
#[tokio::test]
async fn async_broker_negotiates_hello_then_proxies_session() {
    let pid = std::process::id();
    let daemon_path = std::env::temp_dir().join(format!("rp-async-brk-d-{pid}.sock"));
    let broker_path = std::env::temp_dir().join(format!("rp-async-brk-b-{pid}.sock"));
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

    // Broker: async Hello round-trip, then full-proxy to the daemon endpoint.
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
        // The responder points the per-connection relay at the daemon endpoint
        // via Negotiated.backend_pipe — the production resolution path.
        let _ = serve_broker_session_endpoint(
            broker_listener,
            &NegotiateToBackend(daemon_path_str),
            &PeerCredentialPolicy::allow_any(),
        )
        .await;
    });

    // Client dials ONLY the broker: first the Hello frame, then the SESSION wire.
    let mut stream = interprocess::local_socket::tokio::Stream::connect(
        broker_path
            .as_path()
            .to_fs_name::<GenericFilePath>()
            .expect("client fs name"),
    )
    .await
    .expect("client dials broker");

    // 1) Hello round-trip. The responder ignores the payload, so any valid
    //    Frame suffices; what is under test is the framed async exchange.
    let hello_wire =
        encode_framed(&Frame::request(CONTROL_PAYLOAD_PROTOCOL, Vec::new())).expect("encode hello");
    stream.write_all(&hello_wire).await.expect("send hello");
    let reply_body = read_framed_body(&mut stream).await;
    let reply_frame = Frame::decode(reply_body.as_slice()).expect("decode reply frame");
    let reply = HelloReply::decode(reply_frame.payload.as_slice()).expect("decode HelloReply");
    assert!(
        matches!(reply.result, Some(HelloReplyResult::Negotiated(_))),
        "broker negotiated the Hello"
    );

    // 2) Same connection is now a transparent SESSION relay to the daemon.
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

    assert_eq!(
        stdout, b"HELLO",
        "stdout proxied post-Hello across the broker"
    );
    assert_eq!(
        stderr, b"WORLD",
        "stderr proxied post-Hello across the broker"
    );
    assert_eq!(
        code,
        Some(9),
        "exit code proxied after async Hello negotiation"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn async_broker_drops_peer_refused_by_policy() {
    let pid = std::process::id();
    let broker_path = std::env::temp_dir().join(format!("rp-async-brk-drop-{pid}.sock"));
    let _ = std::fs::remove_file(&broker_path);

    let broker_listener = ListenerOptions::new()
        .name(
            broker_path
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .expect("broker fs name"),
        )
        .create_tokio()
        .expect("bind broker endpoint");

    // A policy whose owner can never match a real peer's uid/SID, so every
    // connection is refused on the credential check before any Hello read.
    let policy = PeerCredentialPolicy::owner_only("rp-no-such-owner-sentinel");
    let broker = tokio::spawn(async move {
        // backend_pipe is irrelevant here: the peer is refused before any Hello.
        let _ = serve_broker_session_endpoint(
            broker_listener,
            &NegotiateToBackend(String::new()),
            &policy,
        )
        .await;
    });

    let mut stream = interprocess::local_socket::tokio::Stream::connect(
        broker_path
            .as_path()
            .to_fs_name::<GenericFilePath>()
            .expect("client fs name"),
    )
    .await
    .expect("client dials broker");

    // Send a well-formed Hello; the broker must drop us on the credential check
    // *before* replying, so the reply read hits EOF (the connection is closed).
    let hello_wire =
        encode_framed(&Frame::request(CONTROL_PAYLOAD_PROTOCOL, Vec::new())).expect("encode hello");
    let _ = stream.write_all(&hello_wire).await;
    let mut one = [0u8; 1];
    let read = stream.read_exact(&mut one).await;

    broker.abort();
    let _ = std::fs::remove_file(&broker_path);

    assert!(
        read.is_err(),
        "a peer refused by PeerCredentialPolicy must be dropped without a Hello reply"
    );
}
