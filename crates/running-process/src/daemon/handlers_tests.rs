use super::*;
use crate::proto::daemon::{
    ListByOriginatorRequest, PingRequest, RegisterRequest, RequestType, ShutdownRequest,
    SpawnDaemonRequest, StatusRequest, UnregisterRequest,
};

/// Build a minimal `DaemonState` for testing.
fn test_state() -> (DaemonState, tempfile::TempDir) {
    let (shutdown_tx, _rx) = watch::channel(false);
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let db_path = tmp_dir.path().join("test-handlers.db");
    let registry = Arc::new(Registry::open(&db_path).unwrap());
    let pty_sessions = Arc::new(crate::daemon::pty_sessions::PtySessionRegistry::new());
    let pipe_sessions = Arc::new(crate::daemon::pipe_sessions::PipeSessionRegistry::new());
    let state = DaemonState {
        start_time: Instant::now(),
        version: "0.0.0-test".to_string(),
        socket_path: "/tmp/test.sock".to_string(),
        db_path: "/tmp/test.db".to_string(),
        scope: "global".to_string(),
        scope_hash: "0000000000000000".to_string(),
        scope_cwd: "/tmp".to_string(),
        shutdown_tx,
        active_connections: AtomicU32::new(3),
        registry,
        pty_sessions,
        pipe_sessions,
    };
    (state, tmp_dir)
}

fn make_request(id: u64, rtype: RequestType) -> DaemonRequest {
    DaemonRequest {
        id,
        r#type: rtype as i32,
        protocol_version: 1,
        client_name: "test".to_string(),
        ..Default::default()
    }
}

#[test]
fn ping_returns_ok_with_server_time() {
    let (state, _tmp) = test_state();
    let mut req = make_request(1, RequestType::Ping);
    req.ping = Some(PingRequest {});

    let resp = handle_ping(&req, &state);

    assert_eq!(resp.request_id, 1);
    assert_eq!(resp.code, StatusCode::Ok as i32);
    assert!(resp.ping.is_some());
    assert!(resp.ping.unwrap().server_time_ms > 0);
}

#[test]
fn status_returns_daemon_info() {
    let (state, _tmp) = test_state();
    let mut req = make_request(2, RequestType::Status);
    req.status = Some(StatusRequest {});

    let resp = handle_status(&req, &state);

    assert_eq!(resp.request_id, 2);
    assert_eq!(resp.code, StatusCode::Ok as i32);
    let status = resp.status.unwrap();
    assert_eq!(status.version, "0.0.0-test");
    assert_eq!(status.active_connections, 3);
    assert_eq!(status.socket_path, "/tmp/test.sock");
    assert_eq!(status.db_path, "/tmp/test.db");
    assert_eq!(status.scope, "global");
    assert_eq!(status.scope_hash, "0000000000000000");
    assert_eq!(status.scope_cwd, "/tmp");
}

#[test]
fn shutdown_signals_channel() {
    let (state, _tmp) = test_state();
    // Keep a receiver to check the shutdown signal.
    let rx = state.shutdown_tx.subscribe();
    let mut req = make_request(3, RequestType::Shutdown);
    req.shutdown = Some(ShutdownRequest {
        graceful: true,
        timeout_seconds: 5.0,
    });

    let resp = handle_shutdown(&req, &state);

    assert_eq!(resp.request_id, 3);
    assert_eq!(resp.code, StatusCode::Ok as i32);
    assert_eq!(resp.message, "shutting down");
    assert!(resp.shutdown.is_some());
    // The channel should now hold `true`.
    assert!(rx.has_changed().unwrap_or(false) || *rx.borrow());
}

// -----------------------------------------------------------------------
// Register / Unregister / List handler tests
// -----------------------------------------------------------------------

#[test]
fn test_register_and_list_active() {
    let (state, _tmp) = test_state();
    let mut req = make_request(10, RequestType::Register);
    req.register = Some(RegisterRequest {
        pid: 12345,
        created_at: 1000.5,
        kind: "subprocess".into(),
        command: "sleep 100".into(),
        cwd: "/tmp".into(),
        originator: "test:unit".into(),
        containment: "contained".into(),
    });

    let resp = handle_register(&req, &state);
    assert_eq!(resp.code, StatusCode::Ok as i32);
    assert!(resp.register.is_some());

    // Now list active and verify the registered process appears.
    let list_req = make_request(11, RequestType::ListActive);
    let list_resp = handle_list_active(&list_req, &state);
    assert_eq!(list_resp.code, StatusCode::Ok as i32);

    let active = list_resp.list_active.unwrap();
    assert_eq!(active.processes.len(), 1);

    let proc = &active.processes[0];
    assert_eq!(proc.pid, 12345);
    assert_eq!(proc.command, "sleep 100");
    assert_eq!(proc.kind, "subprocess");
    assert_eq!(proc.originator, "test:unit");
    assert_eq!(proc.containment, "contained");
    assert_eq!(proc.state, ProcessState::Alive as i32);
}

#[test]
fn test_register_missing_payload() {
    let (state, _tmp) = test_state();
    let req = make_request(20, RequestType::Register);
    // No register payload set.

    let resp = handle_register(&req, &state);
    assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
    assert!(resp.message.contains("missing register payload"));
}

#[test]
fn test_register_invalid_pid() {
    let (state, _tmp) = test_state();
    let mut req = make_request(21, RequestType::Register);
    req.register = Some(RegisterRequest {
        pid: 0,
        created_at: 1000.0,
        kind: "subprocess".into(),
        command: "ls".into(),
        ..Default::default()
    });

    let resp = handle_register(&req, &state);
    assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
    assert!(resp.message.contains("pid must be > 0"));
}

#[test]
fn test_register_empty_command() {
    let (state, _tmp) = test_state();
    let mut req = make_request(22, RequestType::Register);
    req.register = Some(RegisterRequest {
        pid: 1,
        created_at: 1000.0,
        kind: "subprocess".into(),
        command: String::new(),
        ..Default::default()
    });

    let resp = handle_register(&req, &state);
    assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
    assert!(resp.message.contains("command must not be empty"));
}

#[test]
fn test_unregister_not_found() {
    let (state, _tmp) = test_state();
    let mut req = make_request(30, RequestType::Unregister);
    req.unregister = Some(UnregisterRequest { pid: 99999 });

    let resp = handle_unregister(&req, &state);
    assert_eq!(resp.code, StatusCode::NotFound as i32);
    assert!(resp.message.contains("99999"));
}

#[test]
fn test_unregister_success() {
    let (state, _tmp) = test_state();

    // Register first.
    let mut reg_req = make_request(31, RequestType::Register);
    reg_req.register = Some(RegisterRequest {
        pid: 5555,
        created_at: 2000.0,
        kind: "subprocess".into(),
        command: "echo hi".into(),
        cwd: "/tmp".into(),
        originator: "test:unit".into(),
        containment: "contained".into(),
    });
    let reg_resp = handle_register(&reg_req, &state);
    assert_eq!(reg_resp.code, StatusCode::Ok as i32);

    // Now unregister.
    let mut unreg_req = make_request(32, RequestType::Unregister);
    unreg_req.unregister = Some(UnregisterRequest { pid: 5555 });

    let unreg_resp = handle_unregister(&unreg_req, &state);
    assert_eq!(unreg_resp.code, StatusCode::Ok as i32);
    assert!(unreg_resp.unregister.is_some());

    // Verify list is now empty.
    let list_req = make_request(33, RequestType::ListActive);
    let list_resp = handle_list_active(&list_req, &state);
    assert_eq!(list_resp.list_active.unwrap().processes.len(), 0);
}

#[test]
fn test_spawn_daemon_missing_payload() {
    let (state, _tmp) = test_state();
    let req = make_request(34, RequestType::SpawnDaemon);

    let resp = handle_spawn_daemon(&req, &state);
    assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
    assert!(resp.message.contains("missing spawn_daemon payload"));
}

#[test]
fn test_spawn_daemon_empty_command() {
    let (state, _tmp) = test_state();
    let mut req = make_request(35, RequestType::SpawnDaemon);
    req.spawn_daemon = Some(SpawnDaemonRequest {
        command: "   ".into(),
        ..Default::default()
    });

    let resp = handle_spawn_daemon(&req, &state);
    assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
    assert!(resp.message.contains("command must not be empty"));
}

#[test]
fn request_type_enum_values_match_convention() {
    // Verify the enum values we use in dispatch match expected values.
    assert_eq!(RequestType::Register as i32, 10);
    assert_eq!(RequestType::Unregister as i32, 11);
    assert_eq!(RequestType::SpawnDaemon as i32, 12);
    assert_eq!(RequestType::ListActive as i32, 20);
    assert_eq!(RequestType::ListByOriginator as i32, 21);
    assert_eq!(RequestType::GetProcessTree as i32, 22);
    assert_eq!(RequestType::KillZombies as i32, 30);
    assert_eq!(RequestType::KillTree as i32, 31);
    assert_eq!(RequestType::Ping as i32, 40);
    assert_eq!(RequestType::Shutdown as i32, 41);
    assert_eq!(RequestType::Status as i32, 42);
}

#[test]
fn test_list_by_originator_filters() {
    let (state, _tmp) = test_state();

    // Register two processes with different originators.
    let mut req1 = make_request(40, RequestType::Register);
    req1.register = Some(RegisterRequest {
        pid: 1001,
        created_at: 1000.0,
        kind: "subprocess".into(),
        command: "cmd1".into(),
        cwd: "/tmp".into(),
        originator: "codeup:session-a".into(),
        containment: "contained".into(),
    });
    assert_eq!(handle_register(&req1, &state).code, StatusCode::Ok as i32);

    let mut req2 = make_request(41, RequestType::Register);
    req2.register = Some(RegisterRequest {
        pid: 1002,
        created_at: 2000.0,
        kind: "pty".into(),
        command: "cmd2".into(),
        cwd: "/home".into(),
        originator: "other:session-b".into(),
        containment: "detached".into(),
    });
    assert_eq!(handle_register(&req2, &state).code, StatusCode::Ok as i32);

    // Filter by "codeup" — should return 1 process.
    let mut list_req = make_request(42, RequestType::ListByOriginator);
    list_req.list_by_originator = Some(ListByOriginatorRequest {
        tool: "codeup".into(),
    });
    let resp = handle_list_by_originator(&list_req, &state);
    assert_eq!(resp.code, StatusCode::Ok as i32);
    let procs = resp.list_by_originator.unwrap().processes;
    assert_eq!(procs.len(), 1);
    assert_eq!(procs[0].pid, 1001);

    // Filter by "other" — should return 1 process.
    let mut list_req2 = make_request(43, RequestType::ListByOriginator);
    list_req2.list_by_originator = Some(ListByOriginatorRequest {
        tool: "other".into(),
    });
    let resp2 = handle_list_by_originator(&list_req2, &state);
    let procs2 = resp2.list_by_originator.unwrap().processes;
    assert_eq!(procs2.len(), 1);
    assert_eq!(procs2[0].pid, 1002);

    // Filter by "nonexistent" — should return 0.
    let mut list_req3 = make_request(44, RequestType::ListByOriginator);
    list_req3.list_by_originator = Some(ListByOriginatorRequest {
        tool: "nonexistent".into(),
    });
    let resp3 = handle_list_by_originator(&list_req3, &state);
    assert_eq!(resp3.list_by_originator.unwrap().processes.len(), 0);
}

#[test]
fn test_list_by_originator_missing_payload() {
    let (state, _tmp) = test_state();
    let req = make_request(45, RequestType::ListByOriginator);

    let resp = handle_list_by_originator(&req, &state);

    assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
    assert!(resp.message.contains("missing list_by_originator payload"));
}

#[test]
fn test_get_process_tree_missing_payload() {
    let (state, _tmp) = test_state();
    let req = make_request(46, RequestType::GetProcessTree);

    let resp = handle_get_process_tree(&req, &state);

    assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
    assert!(resp.message.contains("missing get_process_tree payload"));
}

#[test]
fn test_kill_tree_missing_payload() {
    let (state, _tmp) = test_state();
    let req = make_request(47, RequestType::KillTree);

    let resp = handle_kill_tree(&req, &state);

    assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
    assert!(resp.message.contains("missing kill_tree payload"));
}

#[test]
fn test_kill_zombies_missing_payload() {
    let (state, _tmp) = test_state();
    let req = make_request(48, RequestType::KillZombies);

    let resp = handle_kill_zombies(&req, &state);

    assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
    assert!(resp.message.contains("missing kill_zombies payload"));
}
