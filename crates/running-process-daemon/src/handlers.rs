//! Request handlers for the daemon's IPC protocol.
//!
//! Each handler receives a [`DaemonRequest`] and a shared [`DaemonState`]
//! reference, returning a fully-constructed [`DaemonResponse`].

use running_process_proto::daemon::{
    DaemonRequest, DaemonResponse, GetProcessTreeResponse, KillTreeResponse, KillZombiesResponse,
    ListActiveResponse, ListByOriginatorResponse, PingResponse, ProcessState, RegisterResponse,
    ShutdownResponse, StatusCode, StatusResponse, TrackedProcess, UnregisterResponse, ZombieReport,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;
use sysinfo::{Pid, System};
use tokio::sync::watch;

use crate::reaper;
use crate::registry::{self, Registry, TrackedEntry};

// ---------------------------------------------------------------------------
// Shared daemon state
// ---------------------------------------------------------------------------

/// Shared state accessible by all request handlers.
///
/// Created once when the server starts and wrapped in an `Arc` so that every
/// connection handler can read (and, for atomics, update) it concurrently.
pub struct DaemonState {
    /// When the daemon process started.
    pub start_time: Instant,
    /// Crate / workspace version string.
    pub version: String,
    /// The IPC socket path the daemon is listening on.
    pub socket_path: String,
    /// Path to the SQLite tracking database.
    pub db_path: String,
    /// Human-readable scope name (e.g. project directory).
    pub scope: String,
    /// FNV-1a hash of the scope (used in file/pipe names).
    pub scope_hash: String,
    /// Working directory that produced the scope hash.
    pub scope_cwd: String,
    /// Channel used to signal the server to shut down.
    pub shutdown_tx: watch::Sender<bool>,
    /// Number of currently active client connections.
    pub active_connections: AtomicU32,
    /// SQLite-backed process registry.
    pub registry: Arc<Registry>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Handle a `Ping` request by returning the current server time.
pub fn handle_ping(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    let server_time_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        ping: Some(PingResponse { server_time_ms }),
        ..Default::default()
    }
}

/// Handle a `Status` request by reporting daemon health information.
pub fn handle_status(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let uptime = state.start_time.elapsed().as_secs();
    let active = state.active_connections.load(Ordering::Relaxed);

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        status: Some(StatusResponse {
            version: state.version.clone(),
            uptime_seconds: uptime,
            tracked_process_count: state.registry.count() as u32,
            active_connections: active,
            socket_path: state.socket_path.clone(),
            db_path: state.db_path.clone(),
            scope: state.scope.clone(),
            scope_hash: state.scope_hash.clone(),
            scope_cwd: state.scope_cwd.clone(),
        }),
        ..Default::default()
    }
}

/// Handle a `Shutdown` request by signalling the server to stop.
pub fn handle_shutdown(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let _ = state.shutdown_tx.send(true);

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: "shutting down".to_string(),
        shutdown: Some(ShutdownResponse {}),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Entry ↔ Proto conversion
// ---------------------------------------------------------------------------

/// Convert a [`TrackedEntry`] to a proto [`TrackedProcess`].
fn entry_to_tracked_process(entry: &TrackedEntry) -> TrackedProcess {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let uptime = (now - entry.registered_at).max(0.0);

    TrackedProcess {
        pid: entry.pid,
        created_at: entry.created_at_ms as f64 / 1000.0,
        kind: entry.kind.clone(),
        command: entry.command.clone(),
        cwd: entry.cwd.clone(),
        originator: entry.originator.clone(),
        containment: entry.containment.clone(),
        registered_at: entry.registered_at,
        uptime_seconds: uptime,
        parent_alive: true,                // Phase 4 reaper will validate
        state: ProcessState::Alive as i32, // Phase 4 reaper will validate
        last_validated_at: 0.0,            // Phase 4
    }
}

// ---------------------------------------------------------------------------
// Register / Unregister / List / Tree handlers
// ---------------------------------------------------------------------------

/// Handle a `Register` request by adding a process to the registry.
pub fn handle_register(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let Some(ref req) = request.register else {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "missing register payload".into(),
        );
    };

    if req.pid == 0 {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "pid must be > 0".into(),
        );
    }
    if req.command.is_empty() {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "command must not be empty".into(),
        );
    }

    let created_at_ms = registry::created_at_to_ms(req.created_at);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    let entry = TrackedEntry {
        pid: req.pid,
        created_at_ms,
        kind: req.kind.clone(),
        command: req.command.clone(),
        cwd: req.cwd.clone(),
        originator: req.originator.clone(),
        containment: req.containment.clone(),
        registered_at: now,
    };

    if let Err(e) = state.registry.register(entry) {
        return error_response(
            request.id,
            StatusCode::Internal,
            format!("registry error: {e}"),
        );
    }

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        register: Some(RegisterResponse {}),
        ..Default::default()
    }
}

/// Handle an `Unregister` request by removing a process from the registry.
pub fn handle_unregister(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let Some(ref req) = request.unregister else {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "missing unregister payload".into(),
        );
    };

    if state.registry.unregister(req.pid) {
        DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            unregister: Some(UnregisterResponse {}),
            ..Default::default()
        }
    } else {
        error_response(
            request.id,
            StatusCode::NotFound,
            format!("pid {} not found in registry", req.pid),
        )
    }
}

/// Handle a `ListActive` request by returning all tracked processes.
pub fn handle_list_active(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let entries = state.registry.list_all();
    let processes: Vec<TrackedProcess> = entries.iter().map(entry_to_tracked_process).collect();

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        list_active: Some(ListActiveResponse { processes }),
        ..Default::default()
    }
}

/// Handle a `ListByOriginator` request by returning processes matching the tool prefix.
pub fn handle_list_by_originator(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let Some(ref req) = request.list_by_originator else {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "missing list_by_originator payload".into(),
        );
    };

    let entries = state.registry.list_by_originator(&req.tool);
    let processes: Vec<TrackedProcess> = entries.iter().map(entry_to_tracked_process).collect();

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        list_by_originator: Some(ListByOriginatorResponse { processes }),
        ..Default::default()
    }
}

/// Handle a `GetProcessTree` request by building a tree display string via sysinfo.
pub fn handle_get_process_tree(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    let Some(ref req) = request.get_process_tree else {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "missing get_process_tree payload".into(),
        );
    };

    let tree_display = build_process_tree_display(req.pid);

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        get_process_tree: Some(GetProcessTreeResponse { tree_display }),
        ..Default::default()
    }
}

/// Build a human-readable process tree string rooted at `root_pid` using sysinfo.
fn build_process_tree_display(root_pid: u32) -> String {
    let mut sys = System::new();
    sys.refresh_processes();

    let sysinfo_pid = Pid::from_u32(root_pid);
    let Some(root_proc) = sys.process(sysinfo_pid) else {
        return format!("Process {root_pid} not found");
    };

    let mut lines = Vec::new();
    lines.push(format!(
        "{} (pid={root_pid}) {}",
        root_proc.name(),
        root_proc
            .cmd()
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    ));

    // Collect children recursively.
    fn collect_children(sys: &System, parent_pid: Pid, prefix: &str, lines: &mut Vec<String>) {
        let children: Vec<_> = sys
            .processes()
            .values()
            .filter(|p| p.parent() == Some(parent_pid))
            .collect();

        for (i, child) in children.iter().enumerate() {
            let is_last = i == children.len() - 1;
            let connector = if is_last { "└── " } else { "├── " };
            let child_prefix = if is_last { "    " } else { "│   " };

            lines.push(format!(
                "{prefix}{connector}{} (pid={})",
                child.name(),
                child.pid().as_u32()
            ));

            collect_children(sys, child.pid(), &format!("{prefix}{child_prefix}"), lines);
        }
    }

    collect_children(&sys, sysinfo_pid, "", &mut lines);
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// KillTree handler
// ---------------------------------------------------------------------------

/// Handle a `KillTree` request by killing a process and its descendants.
pub fn handle_kill_tree(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let Some(ref req) = request.kill_tree else {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "missing kill_tree payload".into(),
        );
    };

    let timeout = if req.timeout_seconds > 0.0 {
        req.timeout_seconds
    } else {
        3.0
    };
    let killed = kill_process_tree_impl(req.pid, timeout);

    // Unregister from registry (if tracked).
    state.registry.unregister(req.pid);

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        kill_tree: Some(KillTreeResponse {
            processes_killed: killed,
        }),
        ..Default::default()
    }
}

/// Kill a process tree rooted at `pid`, returning the number of processes killed.
///
/// Collects all descendants via sysinfo, then kills them in reverse order
/// (children before parent) so that parent processes do not respawn children.
fn kill_process_tree_impl(pid: u32, _timeout_seconds: f64) -> u32 {
    use sysinfo::Signal;

    let mut sys = System::new();
    sys.refresh_processes();

    let target = Pid::from_u32(pid);

    // Collect the root and all descendants.
    let mut to_kill = Vec::new();
    collect_descendants(&sys, target, &mut to_kill);
    to_kill.push(target);

    // Kill in reverse order (deepest children first, root last).
    to_kill.reverse();

    let mut killed_count = 0u32;
    for &p in &to_kill {
        if let Some(proc) = sys.process(p) {
            if proc.kill_with(Signal::Kill).unwrap_or(false) {
                killed_count += 1;
            }
        }
    }
    killed_count
}

/// Recursively collect all descendant PIDs of `parent_pid`.
fn collect_descendants(sys: &System, parent_pid: Pid, result: &mut Vec<Pid>) {
    for (child_pid, child_proc) in sys.processes() {
        if child_proc.parent() == Some(parent_pid) {
            result.push(*child_pid);
            collect_descendants(sys, *child_pid, result);
        }
    }
}

// ---------------------------------------------------------------------------
// KillZombies handler
// ---------------------------------------------------------------------------

/// Handle a `KillZombies` request by scanning for and optionally killing zombie processes.
pub fn handle_kill_zombies(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let Some(ref req) = request.kill_zombies else {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "missing kill_zombies payload".into(),
        );
    };

    let zombies = reaper::scan_for_zombies(state);
    let orphan_conhosts = reaper::scan_for_orphan_conhosts();

    let mut reports: Vec<ZombieReport> = Vec::new();

    // Registry-based zombies.
    if req.dry_run {
        reports.extend(zombies.iter().map(|z| ZombieReport {
            pid: z.pid,
            command: z.command.clone(),
            reason: z.reason.clone(),
            killed: false,
        }));
        reports.extend(orphan_conhosts.iter().map(|z| ZombieReport {
            pid: z.pid,
            command: z.command.clone(),
            reason: z.reason.clone(),
            killed: false,
        }));
    } else {
        let reg_results = reaper::kill_zombies(state, &zombies);
        reports.extend(
            zombies
                .iter()
                .zip(reg_results.iter())
                .map(|(z, (_pid, killed))| ZombieReport {
                    pid: z.pid,
                    command: z.command.clone(),
                    reason: z.reason.clone(),
                    killed: *killed,
                }),
        );

        let conhost_results = reaper::kill_conhosts(&orphan_conhosts);
        reports.extend(
            orphan_conhosts
                .iter()
                .zip(conhost_results.iter())
                .map(|(z, (_pid, killed))| ZombieReport {
                    pid: z.pid,
                    command: z.command.clone(),
                    reason: z.reason.clone(),
                    killed: *killed,
                }),
        );
    }

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        kill_zombies: Some(KillZombiesResponse { zombies: reports }),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build an error `DaemonResponse` with no payload.
fn error_response(request_id: u64, code: StatusCode, message: String) -> DaemonResponse {
    DaemonResponse {
        request_id,
        code: code as i32,
        message,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use running_process_proto::daemon::{
        ListByOriginatorRequest, PingRequest, RegisterRequest, RequestType, ShutdownRequest,
        StatusRequest, UnregisterRequest,
    };

    /// Build a minimal `DaemonState` for testing.
    fn test_state() -> (DaemonState, tempfile::TempDir) {
        let (shutdown_tx, _rx) = watch::channel(false);
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let db_path = tmp_dir.path().join("test-handlers.db");
        let registry = Arc::new(Registry::open(&db_path).unwrap());
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
    fn request_type_enum_values_match_convention() {
        // Verify the enum values we use in dispatch match expected values.
        assert_eq!(RequestType::Register as i32, 10);
        assert_eq!(RequestType::Unregister as i32, 11);
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
}
