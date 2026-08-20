use super::*;
use crate::platform::ipc::Listener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

static SOCKET_COUNTER: AtomicU64 = AtomicU64::new(1);

fn socket_path(label: &str) -> String {
    let unique = format!(
        "rp-pipe-{label}-{}-{}",
        std::process::id(),
        SOCKET_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    if std::env::consts::OS == "windows" {
        format!(r"\\.\pipe\{unique}")
    } else {
        std::env::temp_dir()
            .join(format!("{unique}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

fn bind(path: &str) -> Listener {
    let _ = std::fs::remove_file(path);
    Listener::bind(&paths::make_socket_endpoint(path).unwrap()).unwrap()
}

fn response(request: &DaemonRequest, sequence: usize) -> DaemonResponse {
    let mut reply = DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        ..Default::default()
    };
    match sequence {
        0 => {
            assert_eq!(request.r#type, RequestType::SpawnPipeSession as i32);
            let payload = request.spawn_pipe_session.as_ref().unwrap();
            assert_eq!(payload.argv, ["fixture", "arg"]);
            assert_eq!(payload.cwd, "work");
            assert_eq!(payload.originator, "coverage");
            assert!(payload.merge_stderr_into_stdout);
            assert_eq!(payload.environment_policy, 1);
            assert!(!payload.clear_inherited_env);
            reply.spawn_pipe_session = Some(SpawnPipeSessionResponse {
                session_id: "session-7".into(),
                pid: 77,
                created_at: 12.5,
            });
        }
        1 => {
            assert_eq!(request.r#type, RequestType::ListPipeSessions as i32);
            assert_eq!(
                request.list_pipe_sessions.as_ref().unwrap().originator,
                "coverage"
            );
            reply.list_pipe_sessions = Some(ListPipeSessionsResponse {
                sessions: vec![PipeSessionInfo {
                    session_id: "session-7".into(),
                    pid: 77,
                    ..Default::default()
                }],
            });
        }
        2 => {
            assert_eq!(request.r#type, RequestType::DetachPipeStream as i32);
            let payload = request.detach_pipe_stream.as_ref().unwrap();
            assert_eq!(payload.session_id, "session-7");
            assert_eq!(payload.stream, PipeStreamKind::Stdout as i32);
        }
        3 => {
            assert_eq!(request.r#type, RequestType::TerminatePipeSession as i32);
            let payload = request.terminate_pipe_session.as_ref().unwrap();
            assert_eq!(payload.session_id, "session-7");
            assert_eq!(payload.grace_ms, 250);
        }
        4 => {
            assert_eq!(request.r#type, RequestType::WritePipeStdin as i32);
            let payload = request.write_pipe_stdin.as_ref().unwrap();
            assert_eq!(payload.session_id, "session-7");
            assert_eq!(payload.data, b"hello");
            assert!(payload.close);
            reply.write_pipe_stdin = Some(WritePipeStdinResponse { bytes_written: 5 });
        }
        5 => {
            reply.code = StatusCode::NotFound as i32;
            reply.message = "gone".into();
        }
        6 => {}
        _ => unreachable!(),
    }
    reply
}

#[test]
fn pipe_rpc_methods_exchange_expected_wire_payloads_and_map_errors() {
    let path = socket_path("rpc");
    let listener = bind(&path);
    let server = thread::spawn(move || {
        let mut stream = listener.accept().unwrap();
        for sequence in 0..7 {
            let bytes = read_length_prefixed(&mut stream).unwrap();
            let request = DaemonRequest::decode(bytes.as_slice()).unwrap();
            let reply = response(&request, sequence);
            write_length_prefixed(&mut stream, &reply.encode_to_vec()).unwrap();
        }
    });

    let mut client = DaemonClient::connect_to(&path).unwrap();
    let request = PipeSpawnRequest::new(["fixture", "arg"])
        .with_cwd("work")
        .with_envs([("ONLY", "VALUE")])
        .with_originator("coverage")
        .with_environment_policy(crate::EnvironmentPolicy::Inherit)
        .merge_stderr();
    let spawned = client.spawn_pipe_session(&request).unwrap();
    assert_eq!(spawned.session_id, "session-7");
    assert_eq!(spawned.pid, 77);
    assert_eq!(spawned.created_at, 12.5);
    assert_eq!(client.list_pipe_sessions("coverage").unwrap().len(), 1);
    client
        .detach_pipe_stream("session-7", PipeStreamKind::Stdout)
        .unwrap();
    client.terminate_pipe_session("session-7", 250).unwrap();
    assert_eq!(
        client
            .write_pipe_stdin("session-7", b"hello", true)
            .unwrap(),
        5
    );

    assert!(matches!(
        client.list_pipe_sessions("missing"),
        Err(ClientError::Server {
            code: StatusCode::NotFound,
            ref message
        }) if message == "gone"
    ));
    assert!(matches!(
        client.list_pipe_sessions("missing-payload"),
        Err(ClientError::Server {
            code: StatusCode::Internal,
            ..
        })
    ));
    drop(client);
    server.join().unwrap();
    let _ = std::fs::remove_file(path);
}

fn spawn_attachment_server(
    label: &str,
    response_bytes: Vec<u8>,
    frame_bytes: Option<Vec<u8>>,
) -> (String, thread::JoinHandle<DaemonRequest>) {
    let path = socket_path(label);
    let listener = bind(&path);
    let server = thread::spawn(move || {
        let mut stream = listener.accept().unwrap();
        let bytes = read_length_prefixed(&mut stream).unwrap();
        let request = DaemonRequest::decode(bytes.as_slice()).unwrap();
        write_length_prefixed(&mut stream, &response_bytes).unwrap();
        if let Some(frame) = frame_bytes {
            write_length_prefixed(&mut stream, &frame).unwrap();
        }
        request
    });
    (path, server)
}

#[test]
fn pipe_attachment_reads_backlog_and_stream_frame() {
    let reply = DaemonResponse {
        request_id: 1,
        code: StatusCode::Ok as i32,
        attach_pipe_stream: Some(AttachPipeStreamResponse {
            backlog: b"old".to_vec(),
            bytes_missed: 9,
            backlog_truncated: true,
        }),
        ..Default::default()
    };
    let frame = PipeStreamFrame {
        frame: Some(crate::proto::daemon::pipe_stream_frame::Frame::Bytes(
            b"new".to_vec(),
        )),
    };
    let (path, server) = spawn_attachment_server(
        "success",
        reply.encode_to_vec(),
        Some(frame.encode_to_vec()),
    );

    let mut attachment =
        PipeStreamAttachment::attach_to(&path, "session-8", PipeStreamKind::Stderr, true).unwrap();
    assert_eq!(attachment.initial_backlog, b"old");
    assert_eq!(attachment.bytes_missed, 9);
    assert_eq!(attachment.recv_frame().unwrap(), frame);
    drop(attachment);
    let request = server.join().unwrap();
    let payload = request.attach_pipe_stream.unwrap();
    assert_eq!(payload.session_id, "session-8");
    assert_eq!(payload.stream, PipeStreamKind::Stderr as i32);
    assert!(payload.steal);
    let _ = std::fs::remove_file(path);
}

#[test]
fn pipe_attachment_maps_rejection_missing_payload_and_decode_errors() {
    let rejected = DaemonResponse {
        code: StatusCode::NotFound as i32,
        message: "no session".into(),
        ..Default::default()
    };
    let (path, server) = spawn_attachment_server("rejected", rejected.encode_to_vec(), None);
    let error = PipeStreamAttachment::attach_to(&path, "missing", PipeStreamKind::Stdout, false)
        .err()
        .unwrap();
    assert!(matches!(
        error,
        PipeAttachError::Server {
            code: StatusCode::NotFound,
            ref message
        } if message == "no session"
    ));
    assert!(error.to_string().contains("NotFound"));
    server.join().unwrap();
    let _ = std::fs::remove_file(path);

    let missing = DaemonResponse {
        code: StatusCode::Ok as i32,
        ..Default::default()
    };
    let (path, server) = spawn_attachment_server("missing", missing.encode_to_vec(), None);
    assert!(matches!(
        PipeStreamAttachment::attach_to(&path, "session", PipeStreamKind::Stdout, false),
        Err(PipeAttachError::MissingPayload)
    ));
    server.join().unwrap();
    let _ = std::fs::remove_file(path);

    let (path, server) = spawn_attachment_server("decode", vec![0xff], None);
    assert!(matches!(
        PipeStreamAttachment::attach_to(&path, "session", PipeStreamKind::Stdout, false),
        Err(PipeAttachError::Decode(_))
    ));
    server.join().unwrap();
    let _ = std::fs::remove_file(path);
}

#[test]
fn length_prefix_helpers_reject_oversized_or_truncated_frames() {
    let mut framed = Vec::new();
    write_length_prefixed(&mut framed, b"payload").unwrap();
    assert_eq!(
        read_length_prefixed(&mut framed.as_slice()).unwrap(),
        b"payload"
    );

    let oversized = (crate::broker::protocol::MAX_FRAME_BYTES as u32 + 1).to_be_bytes();
    assert_eq!(
        read_length_prefixed(&mut oversized.as_slice())
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(
        read_length_prefixed(&mut [0, 0, 0].as_slice())
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::UnexpectedEof
    );
}
