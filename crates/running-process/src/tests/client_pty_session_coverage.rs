use super::*;
use crate::platform::ipc::Listener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

static SOCKET_COUNTER: AtomicU64 = AtomicU64::new(1);

fn socket_path(label: &str) -> String {
    let unique = format!(
        "rp-pty-{label}-{}-{}",
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

#[test]
fn pty_rpc_methods_exchange_wire_payloads_and_map_failures() {
    let path = socket_path("rpc");
    let listener = bind(&path);
    let server = thread::spawn(move || {
        let mut stream = listener.accept().unwrap();
        for sequence in 0..6 {
            let request =
                DaemonRequest::decode(read_length_prefixed(&mut stream).unwrap().as_slice())
                    .unwrap();
            let mut reply = DaemonResponse {
                request_id: request.id,
                code: StatusCode::Ok as i32,
                ..Default::default()
            };
            match sequence {
                0 => {
                    let payload = request.spawn_pty_session.unwrap();
                    assert_eq!(payload.argv, ["fixture", "arg"]);
                    assert_eq!(payload.cwd, "work");
                    assert_eq!(payload.rows, 31);
                    assert_eq!(payload.cols, 101);
                    assert_eq!(payload.originator, "coverage");
                    assert_eq!(payload.environment_policy, 1);
                    reply.spawn_pty_session = Some(SpawnPtySessionResponse {
                        session_id: "pty-7".into(),
                        pid: 707,
                        created_at: 17.5,
                    });
                }
                1 => {
                    assert_eq!(request.list_pty_sessions.unwrap().originator, "coverage");
                    reply.list_pty_sessions = Some(ListPtySessionsResponse {
                        sessions: vec![PtySessionInfo {
                            session_id: "pty-7".into(),
                            pid: 707,
                            ..Default::default()
                        }],
                    });
                }
                2 => assert_eq!(request.detach_pty_session.unwrap().session_id, "pty-7"),
                3 => {
                    let payload = request.terminate_pty_session.unwrap();
                    assert_eq!(payload.session_id, "pty-7");
                    assert_eq!(payload.grace_ms, 300);
                }
                4 => {
                    reply.code = StatusCode::AlreadyAttached as i32;
                    reply.message = "busy".into();
                }
                5 => {}
                _ => unreachable!(),
            }
            write_length_prefixed(&mut stream, &reply.encode_to_vec()).unwrap();
        }
    });

    let mut client = DaemonClient::connect_to(&path).unwrap();
    let request = PtySpawnRequest::new(["fixture", "arg"])
        .with_cwd("work")
        .with_envs([("ONLY", "VALUE")])
        .with_size(31, 101)
        .with_originator("coverage")
        .with_environment_policy(crate::EnvironmentPolicy::Inherit);
    let spawned = client.spawn_pty_session(&request).unwrap();
    assert_eq!(spawned.session_id, "pty-7");
    assert_eq!(spawned.pid, 707);
    assert_eq!(spawned.created_at, 17.5);
    assert_eq!(client.list_pty_sessions("coverage").unwrap().len(), 1);
    client.detach_pty_session("pty-7").unwrap();
    client.terminate_pty_session("pty-7", 300).unwrap();
    assert!(matches!(
        client.list_pty_sessions("busy"),
        Err(ClientError::Server {
            code: StatusCode::AlreadyAttached,
            ref message
        }) if message == "busy"
    ));
    assert!(matches!(
        client.list_pty_sessions("missing-payload"),
        Err(ClientError::Server {
            code: StatusCode::Internal,
            ..
        })
    ));
    drop(client);
    server.join().unwrap();
    let _ = std::fs::remove_file(path);
}

#[test]
fn pty_attachment_exchanges_output_input_resize_interrupt_and_detach() {
    let path = socket_path("stream");
    let listener = bind(&path);
    let output = PtyStreamFrame {
        frame: Some(crate::proto::daemon::pty_stream_frame::Frame::Output(
            b"new".to_vec(),
        )),
    };
    let output_bytes = output.encode_to_vec();
    let server = thread::spawn(move || {
        let mut stream = listener.accept().unwrap();
        let request =
            DaemonRequest::decode(read_length_prefixed(&mut stream).unwrap().as_slice()).unwrap();
        let reply = DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            attach_pty_session: Some(AttachPtySessionResponse {
                backlog: b"old".to_vec(),
                bytes_missed: 11,
                backlog_truncated: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        write_length_prefixed(&mut stream, &reply.encode_to_vec()).unwrap();
        write_length_prefixed(&mut stream, &output_bytes).unwrap();
        let mut inputs = Vec::new();
        for _ in 0..4 {
            inputs.push(
                PtyInputFrame::decode(read_length_prefixed(&mut stream).unwrap().as_slice())
                    .unwrap(),
            );
        }
        (request, inputs)
    });

    let mut attachment = PtyAttachment::attach_to(&path, "pty-8", 32, 102, true).unwrap();
    assert_eq!(attachment.initial_backlog, b"old");
    assert_eq!(attachment.bytes_missed, 11);
    assert_eq!(attachment.recv_frame().unwrap(), output);
    assert_eq!(
        attachment
            .recv_frame_with_timeout(Duration::from_millis(20))
            .unwrap(),
        None
    );
    attachment.send_input(b"input").unwrap();
    attachment.resize(40, 120).unwrap();
    attachment.send_interrupt().unwrap();
    attachment.detach().unwrap();

    let (request, inputs) = server.join().unwrap();
    let attach = request.attach_pty_session.unwrap();
    assert_eq!(attach.session_id, "pty-8");
    assert_eq!((attach.rows, attach.cols), (32, 102));
    assert!(attach.steal);
    assert!(attach.is_tty);
    assert!(matches!(inputs[0].frame, Some(InputOneof::Input(ref data)) if data == b"input"));
    assert!(
        matches!(inputs[1].frame, Some(InputOneof::Resize(ref size)) if size.rows == 40 && size.cols == 120)
    );
    assert!(matches!(inputs[2].frame, Some(InputOneof::Interrupt(true))));
    assert!(matches!(inputs[3].frame, Some(InputOneof::Detach(true))));
    let _ = std::fs::remove_file(path);
}

fn attachment_error(label: &str, response_bytes: Vec<u8>) -> AttachError {
    let path = socket_path(label);
    let listener = bind(&path);
    let server = thread::spawn(move || {
        let mut stream = listener.accept().unwrap();
        let _ = read_length_prefixed(&mut stream).unwrap();
        write_length_prefixed(&mut stream, &response_bytes).unwrap();
    });
    let capabilities = TerminalCapabilities {
        is_tty: false,
        term: Some("coverage-term".into()),
        terminal_program: Some("coverage".into()),
        graphics: TerminalGraphicsCapabilities::unknown(),
    };
    let error = PtyAttachment::attach_to_with_terminal_capabilities(
        &path,
        "pty",
        24,
        80,
        false,
        capabilities,
    )
    .err()
    .unwrap();
    server.join().unwrap();
    let _ = std::fs::remove_file(path);
    error
}

fn attachment_raw_error(label: &str, response_bytes: Vec<u8>) -> AttachError {
    let path = socket_path(label);
    let listener = bind(&path);
    let server = thread::spawn(move || {
        let mut stream = listener.accept().unwrap();
        let _ = read_length_prefixed(&mut stream).unwrap();
        stream.write_all(&response_bytes).unwrap();
    });
    let error = PtyAttachment::attach_to_with_terminal_capabilities(
        &path,
        "pty",
        24,
        80,
        false,
        TerminalCapabilities {
            is_tty: false,
            term: None,
            terminal_program: None,
            graphics: TerminalGraphicsCapabilities::unknown(),
        },
    )
    .err()
    .unwrap();
    server.join().unwrap();
    let _ = std::fs::remove_file(path);
    error
}

#[test]
fn pty_attachment_maps_server_missing_payload_decode_and_io_errors() {
    let error = attachment_error(
        "rejected",
        DaemonResponse {
            code: StatusCode::NotFound as i32,
            message: "gone".into(),
            ..Default::default()
        }
        .encode_to_vec(),
    );
    assert!(matches!(
        error,
        AttachError::Server {
            code: StatusCode::NotFound,
            ref message
        } if message == "gone"
    ));
    assert!(error.to_string().contains("NotFound"));

    assert!(matches!(
        attachment_error(
            "missing",
            DaemonResponse {
                code: StatusCode::Ok as i32,
                ..Default::default()
            }
            .encode_to_vec()
        ),
        AttachError::MissingPayload
    ));
    assert!(matches!(
        attachment_error("decode", vec![0xff]),
        AttachError::Decode(_)
    ));

    let mut oversized = (crate::broker::protocol::MAX_FRAME_BYTES as u32 + 1)
        .to_be_bytes()
        .to_vec();
    oversized.extend_from_slice(b"ignored");
    assert!(matches!(
        attachment_raw_error("oversized", oversized),
        AttachError::Io(_)
    ));
}
