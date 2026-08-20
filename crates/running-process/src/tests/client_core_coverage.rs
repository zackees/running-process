use super::*;
use crate::platform::ipc::{Listener, Stream};
use crate::proto::daemon::SpawnDaemonResponse;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

static SOCKET_COUNTER: AtomicU64 = AtomicU64::new(1);

fn socket_path() -> String {
    let unique = format!(
        "rp-client-{}-{}",
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

fn read_request(stream: &mut Stream) -> DaemonRequest {
    let mut prefix = [0u8; 4];
    stream.read_exact(&mut prefix).unwrap();
    let mut payload = vec![0u8; u32::from_be_bytes(prefix) as usize];
    stream.read_exact(&mut payload).unwrap();
    DaemonRequest::decode(payload.as_slice()).unwrap()
}

fn write_response(stream: &mut Stream, response: DaemonResponse) {
    let payload = response.encode_to_vec();
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(&payload).unwrap();
}

#[test]
fn core_client_maps_spawn_and_session_administration_responses() {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let endpoint = paths::make_socket_endpoint(&path).unwrap();
    let listener = Listener::bind(&endpoint).unwrap();
    let server = thread::spawn(move || {
        let mut stream = listener.accept().unwrap();
        for sequence in 0..13 {
            let request = read_request(&mut stream);
            let mut reply = DaemonResponse {
                request_id: request.id,
                code: StatusCode::Ok as i32,
                ..Default::default()
            };
            match sequence {
                0 | 1 => {
                    let payload = request.spawn_daemon.unwrap();
                    assert_eq!(payload.command, "echo coverage");
                    assert_eq!(payload.environment_policy, 2);
                    assert!(payload.clear_inherited_env);
                    reply.spawn_daemon = Some(SpawnDaemonResponse {
                        pid: 101 + sequence as u32,
                        created_at: 9.5,
                        command: payload.command,
                        cwd: if sequence == 0 {
                            String::new()
                        } else {
                            "work".into()
                        },
                        originator: if sequence == 0 {
                            String::new()
                        } else {
                            "coverage".into()
                        },
                        containment: "job".into(),
                    });
                }
                2 => {}
                3 => {
                    let payload = request.resize_pty_session.unwrap();
                    assert_eq!((payload.rows, payload.cols), (40, 120));
                    reply.code = StatusCode::NotFound as i32;
                }
                4 => {
                    reply.code = StatusCode::InvalidArgument as i32;
                    reply.message = "bad purge".into();
                }
                5 => {}
                6 => {
                    reply.code = StatusCode::InvalidArgument as i32;
                    reply.message = "bad bulk".into();
                }
                7 => {}
                8 => reply.code = StatusCode::NotFound as i32,
                9 => {
                    reply.code = StatusCode::InvalidArgument as i32;
                    reply.message = "bad backlog".into();
                }
                10 => {}
                11 => {
                    reply.get_session_backlog = Some(GetSessionBacklogResponse {
                        backlog: b"saved".to_vec(),
                        bytes_missed: 2,
                        session_kind: "pipe".into(),
                        ..Default::default()
                    });
                }
                12 => {}
                _ => unreachable!(),
            }
            write_response(&mut stream, reply);
        }
    });

    let request = SpawnCommandRequest::shell("echo coverage")
        .with_cwd("work")
        .with_envs([("A", "1")])
        .with_env("A", "2")
        .with_originator("coverage")
        .with_environment_policy(crate::EnvironmentPolicy::UserBaseline);
    assert_eq!(request.env, [("A".into(), "2".into())]);
    let mut client = DaemonClient::connect_to(&path).unwrap();
    let empty = client.spawn_command(&request).unwrap();
    assert_eq!(empty.pid, 101);
    assert_eq!(empty.cwd, None);
    assert_eq!(empty.originator, None);
    let populated = client.spawn_command(&request).unwrap();
    assert_eq!(populated.pid, 102);
    assert_eq!(populated.cwd.as_deref(), Some("work"));
    assert_eq!(populated.originator.as_deref(), Some("coverage"));
    assert!(matches!(
        client.spawn_command(&request),
        Err(ClientError::Server {
            code: StatusCode::Internal,
            ..
        })
    ));
    assert!(matches!(
        client.resize_pty_session("missing", 40, 120),
        Err(ClientError::Server {
            code: StatusCode::NotFound,
            ..
        })
    ));
    assert!(matches!(
        client.purge_exited_sessions("bad"),
        Err(ClientError::Server {
            code: StatusCode::InvalidArgument,
            ..
        })
    ));
    assert!(matches!(
        client.purge_exited_sessions("missing-payload"),
        Err(ClientError::Server {
            code: StatusCode::Internal,
            ..
        })
    ));
    assert!(matches!(
        client.bulk_terminate_sessions(10, "bad", 100),
        Err(ClientError::Server {
            code: StatusCode::InvalidArgument,
            ..
        })
    ));
    assert!(matches!(
        client.bulk_terminate_sessions(10, "missing-payload", 100),
        Err(ClientError::Server {
            code: StatusCode::Internal,
            ..
        })
    ));
    assert!(client
        .get_session_backlog("missing", PipeStreamKind::Stdout)
        .unwrap()
        .is_none());
    assert!(matches!(
        client.get_session_backlog("bad", PipeStreamKind::Stderr),
        Err(ClientError::Server {
            code: StatusCode::InvalidArgument,
            ..
        })
    ));
    assert!(client
        .get_session_backlog("empty", PipeStreamKind::Stdout)
        .unwrap()
        .is_none());
    let backlog = client
        .get_session_backlog("present", PipeStreamKind::Stdout)
        .unwrap()
        .unwrap();
    assert_eq!(backlog.backlog, b"saved");
    client.resize_pty_session("present", 24, 80).unwrap();
    drop(client);
    server.join().unwrap();
    let _ = std::fs::remove_file(path);
}

#[test]
fn client_errors_expose_messages_and_sources() {
    use std::error::Error as _;

    let connect = ClientError::Connect(std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        "refused",
    ));
    assert!(connect.to_string().contains("failed to connect"));
    assert!(connect.source().is_some());
    let io = ClientError::Io(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe"));
    assert!(io.to_string().contains("daemon I/O"));
    assert!(io.source().is_some());
    let decode = ClientError::Decode(DaemonResponse::decode([0xff].as_slice()).unwrap_err());
    assert!(decode.to_string().contains("failed to decode"));
    assert!(decode.source().is_some());
    let server = ClientError::Server {
        code: StatusCode::NotFound,
        message: "gone".into(),
    };
    assert!(server.to_string().contains("NotFound"));
    assert!(server.source().is_none());
    assert!(ClientError::DaemonNotRunning
        .to_string()
        .contains("running-process broker is not running"));
}
