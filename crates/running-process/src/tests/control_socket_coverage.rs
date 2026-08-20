use super::*;
use crate::broker::protocol::{
    hello_reply::Result as HelloReplyResult, read_frame, AdminReplyKind, FrameKind,
    PayloadEncoding, Refused, ENVELOPE_VERSION, PROTOCOL_VERSION,
};
use crate::broker::server::admin::AdminInodePressure;
use std::io::{self, Cursor, Read, Write};
use std::time::Duration;

struct MockStream {
    input: Cursor<Vec<u8>>,
    output: Vec<u8>,
}

impl MockStream {
    fn empty() -> Self {
        Self {
            input: Cursor::new(Vec::new()),
            output: Vec::new(),
        }
    }

    fn framed(bytes: &[u8]) -> Self {
        let mut input = Vec::new();
        write_frame(&mut input, bytes).unwrap();
        Self {
            input: Cursor::new(input),
            output: Vec::new(),
        }
    }

    fn with_frame(frame: &Frame) -> Self {
        Self::framed(&frame.encode_to_vec())
    }

    fn response_frame(&self) -> Frame {
        let mut bytes = self.output.as_slice();
        Frame::decode(read_frame(&mut bytes).unwrap().as_slice()).unwrap()
    }
}

impl Read for MockStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.input.read(buf)
    }
}

impl Write for MockStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.output.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct RefusingResponder;

impl HelloResponder for RefusingResponder {
    fn handle_frame(&self, _frame: Frame, _peer: PeerIdentity) -> HelloReply {
        refused_reply(ErrorCode::ErrorVersionUnsupported, "coverage refusal", 17)
    }
}

fn peer() -> PeerIdentity {
    PeerIdentity {
        pid: std::process::id(),
        uid_or_sid: "coverage-owner".into(),
    }
}

fn snapshot() -> AdminSnapshot {
    AdminSnapshot {
        broker_instance: "coverage".into(),
        broker_pid: 7,
        generated_at_unix_ms: 11,
        uptime: Duration::from_secs(3),
        accepting_hello: true,
        connections_open: 2,
        backends: Vec::new(),
        spawn_budgets: Vec::new(),
        fd_pressure_demoted: false,
        inode_pressure: AdminInodePressure::default(),
    }
}

fn request_frame(payload_protocol: u32, payload: Vec<u8>) -> Frame {
    Frame {
        envelope_version: u32::from(ENVELOPE_VERSION),
        kind: FrameKind::Request as i32,
        payload_protocol,
        payload,
        request_id: 91,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    }
}

fn admin_frame(verb: i32) -> Frame {
    request_frame(
        ADMIN_PAYLOAD_PROTOCOL,
        AdminRequest {
            verb,
            json: true,
            drain_deadline_ms: 0,
            service_name: String::new(),
            output_path: String::new(),
        }
        .encode_to_vec(),
    )
}

fn refusal(reply: HelloReply) -> Refused {
    match reply.result.unwrap() {
        HelloReplyResult::Refused(value) => value,
        HelloReplyResult::Negotiated(value) => panic!("unexpected negotiation: {value:?}"),
    }
}

fn temp_endpoint(tag: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("control socket tempdir");
    let path = if std::env::consts::OS == "windows" {
        format!(r"\\.\pipe\rp-control-coverage-{tag}-{}", std::process::id())
    } else {
        dir.path()
            .join("owner-private")
            .join(format!("{tag}.sock"))
            .display()
            .to_string()
    };
    (dir, path)
}

fn request_over_socket(socket_path: &str, request: &Frame) -> Result<Frame, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let endpoint =
        crate::platform::ipc::Endpoint::new(socket_path.to_owned()).expect("local socket endpoint");
    let mut stream = loop {
        match crate::platform::ipc::Stream::connect(&endpoint) {
            Ok(stream) => break stream,
            Err(error) if std::time::Instant::now() < deadline => {
                let _ = error;
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(format!("control socket did not become ready: {error}")),
        }
    };
    let bytes = request.encode_to_vec();
    write_frame(&mut stream, &bytes).map_err(|error| format!("write control request: {error}"))?;
    let response =
        read_frame(&mut stream).map_err(|error| format!("read control response: {error}"))?;
    Frame::decode(response.as_slice()).map_err(|error| format!("decode control response: {error}"))
}

#[test]
fn private_dispatch_helpers_cover_admin_decoding_limits_and_bad_reply_payloads() {
    assert_eq!(
        admin_request_verb(&admin_frame(AdminVerb::Status as i32)),
        Some(AdminVerb::Status)
    );
    assert_eq!(admin_request_verb(&admin_frame(999)), None);
    assert_eq!(
        admin_request_verb(&request_frame(ADMIN_PAYLOAD_PROTOCOL, vec![0xff])),
        None
    );

    let one = ControlSocketConnectionLimit::Bounded(NonZeroUsize::new(1).unwrap());
    assert!(one.should_continue(0));
    assert!(!one.should_continue(1));
    assert!(ControlSocketConnectionLimit::Unbounded.should_continue(usize::MAX));

    let malformed_reply = request_frame(ADMIN_PAYLOAD_PROTOCOL, vec![0xff]);
    let mut output = Vec::new();
    assert!(matches!(
        write_admin_response_frame(&mut output, &malformed_reply),
        Err(ControlSocketError::DecodeAdminReply(_))
    ));
    assert!(!output.is_empty());
}

#[test]
fn connection_dispatch_drops_foreign_peers_and_refuses_bad_wire_inputs() {
    let mut dropped = MockStream::empty();
    let result = handle_control_connection_with_peer_policy(
        &mut dropped,
        &RefusingResponder,
        &snapshot,
        peer(),
        &PeerCredentialPolicy::owner_only("someone-else"),
    )
    .unwrap();
    assert_eq!(result, ControlSocketReply::DroppedPeer);
    assert!(dropped.output.is_empty());

    for mut stream in [MockStream::empty(), MockStream::framed(b"not-a-frame")] {
        let result = handle_control_connection_with_peer_policy(
            &mut stream,
            &RefusingResponder,
            &snapshot,
            peer(),
            &PeerCredentialPolicy::allow_any(),
        )
        .unwrap();
        let ControlSocketReply::Hello(reply) = result else {
            panic!("expected refusal")
        };
        assert_eq!(
            ErrorCode::try_from(refusal(reply).code),
            Ok(ErrorCode::ErrorPeerRejected)
        );
        assert_eq!(stream.response_frame().request_id, 0);
    }
}

#[test]
fn connection_dispatch_handles_admin_shutdown_and_oversized_hello() {
    for (verb, shutdown) in [(AdminVerb::Status, false), (AdminVerb::Shutdown, true)] {
        let mut stream = MockStream::with_frame(&admin_frame(verb as i32));
        let result = handle_control_connection_with_peer_policy(
            &mut stream,
            &RefusingResponder,
            &snapshot,
            peer(),
            &PeerCredentialPolicy::allow_any(),
        )
        .unwrap();
        if shutdown {
            assert_eq!(result, ControlSocketReply::ShutdownRequested);
        } else {
            let ControlSocketReply::Admin(reply) = result else {
                panic!("expected admin reply")
            };
            assert_eq!(
                AdminReplyKind::try_from(reply.kind),
                Ok(AdminReplyKind::Json)
            );
        }
        assert_eq!(stream.response_frame().request_id, 91);
    }

    let oversized = request_frame(PROTOCOL_VERSION, vec![b'x'; MAX_HELLO_BYTES + 1]);
    let mut stream = MockStream::with_frame(&oversized);
    let result = handle_control_connection_with_peer_policy(
        &mut stream,
        &RefusingResponder,
        &snapshot,
        peer(),
        &PeerCredentialPolicy::allow_any(),
    )
    .unwrap();
    let ControlSocketReply::Hello(reply) = result else {
        panic!("expected oversized refusal")
    };
    assert!(refusal(reply).reason.contains("exceeds 64 KiB"));
    assert_eq!(stream.response_frame().request_id, 91);
}

#[test]
fn zero_connection_server_returns_without_binding() {
    serve_control_socket_connections_with_policy(
        "this-path-must-never-be-bound",
        &RefusingResponder,
        snapshot,
        0,
        &PeerCredentialPolicy::allow_any(),
    )
    .unwrap();
}

#[test]
fn bounded_control_server_accepts_hello_and_admin_connections() {
    let (_dir, path) = temp_endpoint("bounded");
    std::thread::scope(|scope| {
        let server_path = path.clone();
        let server = scope.spawn(move || {
            serve_control_socket_connections_with_policy(
                &server_path,
                &RefusingResponder,
                snapshot,
                2,
                &PeerCredentialPolicy::allow_any(),
            )
        });

        let hello = request_over_socket(&path, &request_frame(PROTOCOL_VERSION, Vec::new()));
        let status = request_over_socket(&path, &admin_frame(AdminVerb::Status as i32));
        let server_result = server.join().unwrap();
        server_result.unwrap();
        assert_eq!(hello.unwrap().request_id, 91);
        assert_eq!(status.unwrap().request_id, 91);
    });
}

#[test]
fn post_hello_hook_observes_a_live_negotiation() {
    let (_dir, path) = temp_endpoint("post-hello");
    let hook_calls = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let server_path = path.clone();
        let hook_calls = &hook_calls;
        let server = scope.spawn(move || {
            serve_control_socket_connections_with_limit_policy_and_post_hello(
                &server_path,
                &RefusingResponder,
                snapshot,
                ControlSocketConnectionLimit::Bounded(NonZeroUsize::new(1).unwrap()),
                &PeerCredentialPolicy::allow_any(),
                |_stream, reply| {
                    assert!(matches!(reply.result, Some(HelloReplyResult::Refused(_))));
                    hook_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                },
            )
        });

        let reply = request_over_socket(&path, &request_frame(PROTOCOL_VERSION, Vec::new()));
        let server_result = server.join().unwrap();
        server_result.unwrap();
        assert_eq!(reply.unwrap().request_id, 91);
    });
    assert_eq!(hook_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn concurrent_control_server_shutdown_acks_and_wakes_accept_loop() {
    let (_dir, path) = temp_endpoint("concurrent-shutdown");
    let guard = FdPressureGuard::default();
    std::thread::scope(|scope| {
        let server_path = path.clone();
        let guard = &guard;
        let server = scope.spawn(move || {
            serve_launch_control_socket_connections_concurrently(
                &server_path,
                &RefusingResponder,
                snapshot,
                ControlSocketConnectionLimit::Bounded(NonZeroUsize::new(2).unwrap()),
                &PeerCredentialPolicy::allow_any(),
                guard,
            )
        });

        let reply = request_over_socket(&path, &admin_frame(AdminVerb::Shutdown as i32));
        // A second best-effort wake guarantees the bounded server can leave
        // accept even if the shutdown request was accepted but its worker
        // failed before setting the shared flag.
        wake_control_socket_accept(&path);
        wake_control_socket_accept(&path);
        let server_result = server.join().unwrap();
        server_result.unwrap();
        assert_eq!(reply.unwrap().request_id, 91);
    });
}
