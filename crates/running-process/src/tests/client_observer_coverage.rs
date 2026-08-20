use super::*;
use crate::client::paths;
use crate::proto::daemon::{
    DaemonResponse, GetSessionObserverStatusResponse, RegisterSessionObserverResponse,
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
        "rp-observer-{}-{}",
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
fn observer_rpc_round_trips_cover_registration_status_and_failures() {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = ListenerOptions::new()
        .name(paths::make_socket_name(&path).unwrap())
        .create_sync()
        .unwrap();
    let server = thread::spawn(move || {
        let mut stream = listener.accept().unwrap();
        for sequence in 0..6 {
            let request = read_request(&mut stream);
            let mut reply = DaemonResponse {
                request_id: request.id,
                code: StatusCode::Ok as i32,
                ..Default::default()
            };
            match sequence {
                0 => {
                    let payload = request.register_session_observer.unwrap();
                    assert_eq!(payload.session_id, "session-1");
                    assert_eq!(payload.session_kind, ProtoObserverSessionKind::Pipe as i32);
                    assert_eq!(payload.categories, [0, 1, 2, 3]);
                    assert_eq!(payload.ring_capacity_events, 17);
                    assert_eq!(
                        payload.backpressure,
                        ProtoObserverBackpressure::Block as i32
                    );
                    reply.register_session_observer = Some(RegisterSessionObserverResponse {
                        subscriber_id: "subscriber-1".into(),
                    });
                }
                1 => {
                    let payload = request.get_session_observer_status.unwrap();
                    assert_eq!(payload.session_id, "session-1");
                    assert_eq!(payload.subscriber_id, "subscriber-1");
                    reply.get_session_observer_status = Some(GetSessionObserverStatusResponse {
                        missed_events: 3,
                        disconnected: true,
                        delivered_events: 9,
                    });
                }
                2 => {
                    let payload = request.unregister_session_observer.unwrap();
                    assert_eq!(payload.session_id, "session-1");
                    assert_eq!(payload.subscriber_id, "subscriber-1");
                }
                3 => {
                    reply.code = StatusCode::NotFound as i32;
                    reply.message = "gone".into();
                }
                4 | 5 => {}
                _ => unreachable!(),
            }
            write_response(&mut stream, reply);
        }
    });

    let mut client = DaemonClient::connect_to(&path).unwrap();
    let request = SessionObserverRequest::new("session-1", SessionObserverKind::Pipe)
        .categories([
            EventCategory::Lifecycle,
            EventCategory::File,
            EventCategory::Network,
            EventCategory::Process,
        ])
        .ring_capacity_events(17)
        .backpressure(SessionObserverBackpressure::Block);
    let subscription = client.register_session_observer(&request).unwrap();
    assert_eq!(subscription.subscriber_id, "subscriber-1");
    assert!(subscription.subscriber.try_recv().is_none());
    let status = client
        .get_session_observer_status(SessionObserverKind::Pipe, "session-1", "subscriber-1")
        .unwrap();
    assert_eq!(status.missed_events, 3);
    assert!(status.disconnected);
    assert_eq!(status.delivered_events, 9);
    client
        .unregister_session_observer(SessionObserverKind::Pty, "session-1", "subscriber-1")
        .unwrap();

    assert!(matches!(
        client.unregister_session_observer(SessionObserverKind::Pipe, "missing", "missing"),
        Err(ClientError::Server {
            code: StatusCode::NotFound,
            ref message
        }) if message == "gone"
    ));
    assert!(matches!(
        client.register_session_observer(&request),
        Err(ClientError::Server {
            code: StatusCode::Internal,
            ..
        })
    ));
    assert!(matches!(
        client.get_session_observer_status(SessionObserverKind::Pipe, "session-1", "missing"),
        Err(ClientError::Server {
            code: StatusCode::Internal,
            ..
        })
    ));
    drop(client);
    server.join().unwrap();
    let _ = std::fs::remove_file(path);
}
