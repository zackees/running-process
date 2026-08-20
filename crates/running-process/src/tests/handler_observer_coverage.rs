use super::*;
use crate::daemon::emergency_reserve::EmergencyReserve;
use crate::daemon::pipe_sessions::PipeSessionRegistry;
use crate::daemon::pty_sessions::PtySessionRegistry;
use crate::daemon::registry::Registry;
use crate::daemon::services::ServiceRegistry;
use crate::proto::daemon::{
    GetSessionObserverStatusRequest, RegisterSessionObserverRequest,
    UnregisterSessionObserverRequest,
};
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::watch;

fn state() -> (tempfile::TempDir, DaemonState) {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("observer.db");
    let (shutdown_tx, _shutdown_rx) = watch::channel(false);
    let state = DaemonState {
        start_time: Instant::now(),
        version: "coverage".into(),
        socket_path: "coverage.sock".into(),
        db_path: db.to_string_lossy().into_owned(),
        scope: "coverage".into(),
        scope_hash: "hash".into(),
        scope_cwd: temp.path().to_string_lossy().into_owned(),
        shutdown_tx,
        active_connections: AtomicU32::new(0),
        registry: Arc::new(Registry::open(&db).unwrap()),
        pty_sessions: Arc::new(PtySessionRegistry::new()),
        pipe_sessions: Arc::new(PipeSessionRegistry::new()),
        services: Arc::new(ServiceRegistry::open(&db, temp.path().join("services")).unwrap()),
        emergency_reserve: Arc::new(EmergencyReserve::initialize_at(
            temp.path().join("reserve.bin"),
            4096,
        )),
    };
    (temp, state)
}

fn register(kind: i32) -> DaemonRequest {
    DaemonRequest {
        id: 19,
        register_session_observer: Some(RegisterSessionObserverRequest {
            session_id: "missing".into(),
            session_kind: kind,
            categories: vec![0, 1, 2, 3, 1],
            ring_capacity_events: 0,
            backpressure: ProtoObserverBackpressure::DropOldest as i32,
        }),
        ..Default::default()
    }
}

fn assert_code(response: DaemonResponse, expected: StatusCode, text: &str) {
    assert_eq!(response.request_id, 19);
    assert_eq!(response.code, expected as i32);
    assert!(response.message.contains(text), "{}", response.message);
}

#[test]
fn category_decoder_defaults_deduplicates_and_rejects_unknown_values() {
    assert_eq!(decode_categories(&[]).unwrap(), [EventCategory::Lifecycle]);
    assert_eq!(
        decode_categories(&[0, 1, 2, 3, 1]).unwrap(),
        [
            EventCategory::Lifecycle,
            EventCategory::File,
            EventCategory::Network,
            EventCategory::Process,
        ]
    );
    assert_eq!(
        decode_categories(&[4]).unwrap_err(),
        "invalid observer category 4"
    );
}

#[test]
fn register_rejects_every_invalid_shape_and_distinguishes_session_kinds() {
    let (_temp, state) = state();
    assert_code(
        handle_register_session_observer(
            &DaemonRequest {
                id: 19,
                ..Default::default()
            },
            &state,
        ),
        StatusCode::InvalidArgument,
        "payload missing",
    );

    let mut request = register(ObserverSessionKind::Pty as i32);
    request
        .register_session_observer
        .as_mut()
        .unwrap()
        .session_id
        .clear();
    assert_code(
        handle_register_session_observer(&request, &state),
        StatusCode::InvalidArgument,
        "session_id",
    );

    let mut request = register(ObserverSessionKind::Pty as i32);
    request
        .register_session_observer
        .as_mut()
        .unwrap()
        .session_kind = 999;
    assert_code(
        handle_register_session_observer(&request, &state),
        StatusCode::InvalidArgument,
        "session kind",
    );

    let mut request = register(ObserverSessionKind::Pty as i32);
    request
        .register_session_observer
        .as_mut()
        .unwrap()
        .categories = vec![99];
    assert_code(
        handle_register_session_observer(&request, &state),
        StatusCode::InvalidArgument,
        "category",
    );

    let mut request = register(ObserverSessionKind::Pty as i32);
    request
        .register_session_observer
        .as_mut()
        .unwrap()
        .backpressure = 999;
    assert_code(
        handle_register_session_observer(&request, &state),
        StatusCode::InvalidArgument,
        "backpressure",
    );

    for (kind, message, expected) in [
        (
            ObserverSessionKind::Pty as i32,
            "PTY session",
            StatusCode::NotFound,
        ),
        (
            ObserverSessionKind::Pipe as i32,
            "pipe session",
            StatusCode::NotFound,
        ),
        (
            ObserverSessionKind::Unspecified as i32,
            "PTY or PIPE",
            StatusCode::InvalidArgument,
        ),
    ] {
        assert_code(
            handle_register_session_observer(&register(kind), &state),
            expected,
            message,
        );
    }

    let mut block = register(ObserverSessionKind::Pipe as i32);
    let payload = block.register_session_observer.as_mut().unwrap();
    payload.backpressure = ProtoObserverBackpressure::Block as i32;
    payload.ring_capacity_events = 7;
    assert_code(
        handle_register_session_observer(&block, &state),
        StatusCode::NotFound,
        "pipe session",
    );
}

#[test]
fn unregister_and_status_reject_missing_invalid_empty_and_unknown_sessions() {
    let (_temp, state) = state();
    assert_code(
        handle_unregister_session_observer(
            &DaemonRequest {
                id: 19,
                ..Default::default()
            },
            &state,
        ),
        StatusCode::InvalidArgument,
        "payload missing",
    );
    assert_code(
        handle_get_session_observer_status(
            &DaemonRequest {
                id: 19,
                ..Default::default()
            },
            &state,
        ),
        StatusCode::InvalidArgument,
        "payload missing",
    );

    for kind in [
        999,
        ObserverSessionKind::Unspecified as i32,
        ObserverSessionKind::Pty as i32,
        ObserverSessionKind::Pipe as i32,
    ] {
        for empty_subscriber in [true, false] {
            let subscriber_id = if empty_subscriber { "" } else { "unknown" };
            let unregister = DaemonRequest {
                id: 19,
                unregister_session_observer: Some(UnregisterSessionObserverRequest {
                    session_id: "missing".into(),
                    session_kind: kind,
                    subscriber_id: subscriber_id.into(),
                }),
                ..Default::default()
            };
            let status = DaemonRequest {
                id: 19,
                get_session_observer_status: Some(GetSessionObserverStatusRequest {
                    session_id: "missing".into(),
                    session_kind: kind,
                    subscriber_id: subscriber_id.into(),
                }),
                ..Default::default()
            };
            let expected = if kind == 999
                || kind == ObserverSessionKind::Unspecified as i32
                || empty_subscriber
            {
                StatusCode::InvalidArgument
            } else {
                StatusCode::NotFound
            };
            assert_eq!(
                handle_unregister_session_observer(&unregister, &state).code,
                expected as i32
            );
            assert_eq!(
                handle_get_session_observer_status(&status, &state).code,
                expected as i32
            );
        }
    }
}
