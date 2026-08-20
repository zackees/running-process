use super::*;
use crate::client::paths;
use crate::proto::daemon::{
    DaemonResponse, GetSessionTeeStatusResponse, RegisterSessionTeeResponse,
};
use interprocess::local_socket::traits::Listener as _;
use interprocess::local_socket::ListenerOptions;
use prost::Message;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

static SOCKET_COUNTER: AtomicU64 = AtomicU64::new(1);

fn socket_path() -> String {
    let unique = format!(
        "rp-tee-{}-{}",
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

fn read_request(stream: &mut interprocess::local_socket::Stream) -> DaemonRequest {
    let mut prefix = [0u8; 4];
    stream.read_exact(&mut prefix).unwrap();
    let mut payload = vec![0u8; u32::from_be_bytes(prefix) as usize];
    stream.read_exact(&mut payload).unwrap();
    DaemonRequest::decode(payload.as_slice()).unwrap()
}

fn write_response(stream: &mut interprocess::local_socket::Stream, response: DaemonResponse) {
    let payload = response.encode_to_vec();
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(&payload).unwrap();
}

#[test]
fn tee_rpc_round_trips_cover_status_and_protocol_failures() {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = ListenerOptions::new()
        .name(paths::make_socket_name(&path).unwrap())
        .create_sync()
        .unwrap();
    let server = thread::spawn(move || {
        let mut stream = listener.accept().unwrap();
        for sequence in 0..8 {
            let request = read_request(&mut stream);
            let mut reply = DaemonResponse {
                request_id: request.id,
                code: StatusCode::Ok as i32,
                ..Default::default()
            };
            match sequence {
                0 => {
                    let payload = request.register_session_tee.unwrap();
                    assert_eq!(payload.session_id, "session-1");
                    assert_eq!(payload.session_kind, ProtoTeeSessionKind::Pipe as i32);
                    assert_eq!(payload.stream, ProtoTeeStreamKind::Stderr as i32);
                    assert_eq!(payload.sink_kind, TeeSinkKind::File as i32);
                    assert!(!payload.file_path.is_empty());
                    assert_eq!(payload.file_mode, ProtoTeeFileMode::Truncate as i32);
                    assert_eq!(payload.queue_capacity, 19);
                    assert!(payload.suppress_missed_markers);
                    assert_eq!(payload.backpressure, ProtoTeeBackpressure::Block as i32);
                    reply.register_session_tee =
                        Some(RegisterSessionTeeResponse { tee_handle: 42 });
                }
                1 => {
                    let payload = request.get_session_tee_status.unwrap();
                    assert_eq!(payload.tee_handle, 42);
                    reply.get_session_tee_status = Some(GetSessionTeeStatusResponse {
                        stream: ProtoTeeStreamKind::Stderr as i32,
                        missed_bytes: 7,
                        disconnected: true,
                    });
                }
                2 => {
                    let payload = request.unregister_session_tee.unwrap();
                    assert_eq!(payload.tee_handle, 42);
                    assert_eq!(payload.session_kind, ProtoTeeSessionKind::Pty as i32);
                }
                3 => {
                    reply.code = StatusCode::NotFound as i32;
                    reply.message = "gone".into();
                }
                4 => {}
                5 => {
                    reply.get_session_tee_status = Some(GetSessionTeeStatusResponse {
                        stream: ProtoTeeStreamKind::Unspecified as i32,
                        ..Default::default()
                    });
                }
                6 => {
                    reply.get_session_tee_status = Some(GetSessionTeeStatusResponse {
                        stream: i32::MAX,
                        ..Default::default()
                    });
                }
                7 => {}
                _ => unreachable!(),
            }
            write_response(&mut stream, reply);
        }
    });

    let request = SessionTeeFileRequest::new(
        "session-1",
        SessionTeeKind::Pipe,
        SessionTeeStream::Stderr,
        "coverage.log",
    )
    .truncate()
    .queue_capacity(19)
    .suppress_missed_markers()
    .backpressure(SessionTeeBackpressure::Block);
    let mut client = DaemonClient::connect_to(&path).unwrap();
    assert_eq!(client.register_session_file_tee(&request).unwrap(), 42);
    let status = client
        .get_session_tee_status(SessionTeeKind::Pipe, "session-1", 42)
        .unwrap();
    assert_eq!(status.stream, SessionTeeStream::Stderr);
    assert_eq!(status.missed_bytes, 7);
    assert!(status.disconnected);
    client
        .unregister_session_tee(SessionTeeKind::Pty, "session-1", 42)
        .unwrap();

    assert!(matches!(
        client.unregister_session_tee(SessionTeeKind::Pipe, "missing", 99),
        Err(ClientError::Server {
            code: StatusCode::NotFound,
            ref message
        }) if message == "gone"
    ));
    assert!(matches!(
        client.register_session_file_tee(&request),
        Err(ClientError::Server {
            code: StatusCode::Internal,
            ..
        })
    ));
    for handle in [1, 2, 3] {
        assert!(matches!(
            client.get_session_tee_status(SessionTeeKind::Pipe, "session-1", handle),
            Err(ClientError::Server {
                code: StatusCode::Internal,
                ..
            })
        ));
    }
    drop(client);
    server.join().unwrap();
    let _ = std::fs::remove_file(path);
}

#[test]
fn tee_enum_mappings_cover_every_stream_and_default() {
    assert_eq!(
        proto_session_kind(SessionTeeKind::Pty),
        ProtoTeeSessionKind::Pty
    );
    assert_eq!(
        proto_session_kind(SessionTeeKind::Pipe),
        ProtoTeeSessionKind::Pipe
    );
    for (client, proto) in [
        (SessionTeeStream::PtyOutput, ProtoTeeStreamKind::PtyOutput),
        (SessionTeeStream::Stdout, ProtoTeeStreamKind::Stdout),
        (SessionTeeStream::Stderr, ProtoTeeStreamKind::Stderr),
        (SessionTeeStream::Stdin, ProtoTeeStreamKind::Stdin),
    ] {
        assert_eq!(proto_stream_kind(client), proto);
        assert_eq!(client_stream_kind(proto).unwrap(), client);
    }
    assert_eq!(
        proto_file_mode(SessionTeeFileMode::Append),
        ProtoTeeFileMode::Append
    );
    assert_eq!(
        proto_file_mode(SessionTeeFileMode::Truncate),
        ProtoTeeFileMode::Truncate
    );
    assert_eq!(
        proto_backpressure(SessionTeeBackpressure::DropOldest),
        ProtoTeeBackpressure::DropOldest
    );
    assert_eq!(
        proto_backpressure(SessionTeeBackpressure::Block),
        ProtoTeeBackpressure::Block
    );
    assert!(matches!(
        client_stream_kind(ProtoTeeStreamKind::Unspecified),
        Err(ClientError::Server {
            code: StatusCode::Internal,
            ..
        })
    ));
}
