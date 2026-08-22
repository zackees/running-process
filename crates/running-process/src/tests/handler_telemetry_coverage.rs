use super::*;
use crate::daemon::emergency_reserve::EmergencyReserve;
use crate::daemon::pipe_sessions::PipeSessionRegistry;
use crate::daemon::pty_sessions::PtySessionRegistry;
use crate::daemon::registry::Registry;
use crate::daemon::services::ServiceRegistry;
use crate::proto::daemon::{
    GetSessionTeeStatusRequest, RegisterSessionTeeRequest, UnregisterSessionTeeRequest,
};
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::watch;

fn state() -> (DaemonState, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("handlers.db");
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
    (state, temp)
}

fn register_request() -> DaemonRequest {
    DaemonRequest {
        id: 10,
        register_session_tee: Some(RegisterSessionTeeRequest {
            session_id: "missing".into(),
            session_kind: TeeSessionKind::Pty as i32,
            stream: TeeStreamKind::PtyOutput as i32,
            sink_kind: TeeSinkKind::File as i32,
            file_path: crate::platform::fs::encode_path_bytes(std::path::Path::new("coverage.log")),
            file_mode: ProtoTeeFileMode::Append as i32,
            queue_capacity: 0,
            suppress_missed_markers: false,
            backpressure: ProtoTeeBackpressure::DropOldest as i32,
        }),
        ..Default::default()
    }
}

fn assert_code(response: DaemonResponse, code: StatusCode, message: &str) {
    assert_eq!(response.request_id, 10);
    assert_eq!(response.code, code as i32);
    assert!(response.message.contains(message), "{}", response.message);
}

#[test]
fn register_handler_rejects_each_invalid_wire_shape() {
    let (state, _temp) = state();
    assert_code(
        handle_register_session_tee(
            &DaemonRequest {
                id: 10,
                ..Default::default()
            },
            &state,
        ),
        StatusCode::InvalidArgument,
        "payload missing",
    );

    let mut request = register_request();
    request
        .register_session_tee
        .as_mut()
        .unwrap()
        .session_id
        .clear();
    assert_code(
        handle_register_session_tee(&request, &state),
        StatusCode::InvalidArgument,
        "session_id",
    );

    let mut request = register_request();
    request.register_session_tee.as_mut().unwrap().sink_kind = 999;
    assert_code(
        handle_register_session_tee(&request, &state),
        StatusCode::InvalidArgument,
        "sink kind",
    );
    let mut request = register_request();
    request.register_session_tee.as_mut().unwrap().sink_kind = TeeSinkKind::Unspecified as i32;
    assert_code(
        handle_register_session_tee(&request, &state),
        StatusCode::InvalidArgument,
        "only file",
    );

    let mut request = register_request();
    request.register_session_tee.as_mut().unwrap().stream = 999;
    assert_code(
        handle_register_session_tee(&request, &state),
        StatusCode::InvalidArgument,
        "invalid tee stream",
    );
    let mut request = register_request();
    request
        .register_session_tee
        .as_mut()
        .unwrap()
        .file_path
        .clear();
    assert_code(
        handle_register_session_tee(&request, &state),
        StatusCode::InvalidArgument,
        "must not be empty",
    );

    for (field, message) in [("mode", "file mode"), ("backpressure", "backpressure")] {
        let mut request = register_request();
        let payload = request.register_session_tee.as_mut().unwrap();
        if field == "mode" {
            payload.file_mode = 999;
        } else {
            payload.backpressure = 999;
        }
        assert_code(
            handle_register_session_tee(&request, &state),
            StatusCode::InvalidArgument,
            message,
        );
    }

    let mut request = register_request();
    request.register_session_tee.as_mut().unwrap().session_kind = 999;
    assert_code(
        handle_register_session_tee(&request, &state),
        StatusCode::InvalidArgument,
        "session kind",
    );
    let mut request = register_request();
    request.register_session_tee.as_mut().unwrap().session_kind =
        TeeSessionKind::Unspecified as i32;
    assert_code(
        handle_register_session_tee(&request, &state),
        StatusCode::InvalidArgument,
        "PTY or PIPE",
    );
}

#[test]
fn register_handler_distinguishes_missing_sessions_and_wrong_streams() {
    let (state, _temp) = state();
    let request = register_request();
    assert_code(
        handle_register_session_tee(&request, &state),
        StatusCode::NotFound,
        "PTY session",
    );

    let mut request = register_request();
    let payload = request.register_session_tee.as_mut().unwrap();
    payload.session_kind = TeeSessionKind::Pipe as i32;
    payload.stream = TeeStreamKind::Stdout as i32;
    assert_code(
        handle_register_session_tee(&request, &state),
        StatusCode::NotFound,
        "pipe session",
    );

    assert!(matches!(
        register_pty_file_tee(
            &state,
            "missing",
            TeeStreamKind::Stdout,
            &PathBuf::from("x"),
            TeeFileOptions::default(),
        ),
        Err(RegistrationError::NotFound(_))
    ));
    assert!(matches!(
        register_pipe_file_tee(
            &state,
            "missing",
            TeeStreamKind::PtyOutput,
            &PathBuf::from("x"),
            TeeFileOptions::default(),
        ),
        Err(RegistrationError::NotFound(_))
    ));
}

#[test]
fn unregister_and_status_handlers_cover_missing_invalid_and_each_session_kind() {
    let (state, _temp) = state();
    assert_code(
        handle_unregister_session_tee(
            &DaemonRequest {
                id: 10,
                ..Default::default()
            },
            &state,
        ),
        StatusCode::InvalidArgument,
        "payload missing",
    );
    assert_code(
        handle_get_session_tee_status(
            &DaemonRequest {
                id: 10,
                ..Default::default()
            },
            &state,
        ),
        StatusCode::InvalidArgument,
        "payload missing",
    );

    for kind in [
        999,
        TeeSessionKind::Unspecified as i32,
        TeeSessionKind::Pty as i32,
        TeeSessionKind::Pipe as i32,
    ] {
        let unregister = DaemonRequest {
            id: 10,
            unregister_session_tee: Some(UnregisterSessionTeeRequest {
                session_id: "missing".into(),
                session_kind: kind,
                tee_handle: 7,
            }),
            ..Default::default()
        };
        let status = DaemonRequest {
            id: 10,
            get_session_tee_status: Some(GetSessionTeeStatusRequest {
                session_id: "missing".into(),
                session_kind: kind,
                tee_handle: 7,
            }),
            ..Default::default()
        };
        let expected = if kind == 999 || kind == TeeSessionKind::Unspecified as i32 {
            StatusCode::InvalidArgument
        } else {
            StatusCode::NotFound
        };
        assert_eq!(
            handle_unregister_session_tee(&unregister, &state).code,
            expected as i32
        );
        assert_eq!(
            handle_get_session_tee_status(&status, &state).code,
            expected as i32
        );
    }
}

#[test]
fn option_status_path_and_error_conversions_cover_all_variants() {
    let append = file_options(
        ProtoTeeFileMode::Append as i32,
        0,
        false,
        ProtoTeeBackpressure::DropOldest as i32,
    )
    .unwrap();
    assert_eq!(append, TeeFileOptions::default());
    let custom = file_options(
        ProtoTeeFileMode::Truncate as i32,
        7,
        true,
        ProtoTeeBackpressure::Block as i32,
    )
    .unwrap();
    assert_eq!(custom.mode, TeeFileMode::Truncate);
    assert_eq!(custom.queue_capacity, 7);
    assert!(!custom.write_missed_markers);
    assert_eq!(custom.backpressure, TeeBackpressure::Block);

    for (stream, expected) in [
        (TeeStream::PtyOutput, TeeStreamKind::PtyOutput),
        (TeeStream::Stdout, TeeStreamKind::Stdout),
        (TeeStream::Stderr, TeeStreamKind::Stderr),
        (TeeStream::Stdin, TeeStreamKind::Stdin),
    ] {
        let response = status_response(TeeStatus {
            stream,
            missed_bytes: 9,
            disconnected: true,
        });
        assert_eq!(response.stream, expected as i32);
        assert_eq!(response.missed_bytes, 9);
        assert!(response.disconnected);
    }

    let round_tripped = crate::platform::fs::decode_path_bytes(
        &crate::platform::fs::encode_path_bytes(std::path::Path::new("x")),
    )
    .expect("decode what the facade encoded");
    assert_eq!(round_tripped, std::path::Path::new("x"));
    let invalid = RegistrationError::from_io(io::Error::new(io::ErrorKind::InvalidInput, "bad"));
    assert_eq!(
        invalid.into_response(10).code,
        StatusCode::InvalidArgument as i32
    );
    let internal = RegistrationError::from_io(io::Error::other("broken"));
    assert_eq!(internal.into_response(10).code, StatusCode::Internal as i32);
}
