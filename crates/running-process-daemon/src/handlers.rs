//! Request handlers for the daemon's IPC protocol.
//!
//! Each handler receives a [`DaemonRequest`] and a shared [`DaemonState`]
//! reference, returning a fully-constructed [`DaemonResponse`].

use running_process_core::ORIGINATOR_ENV_VAR;
use running_process_proto::daemon::{
    DaemonRequest, DaemonResponse, GetProcessTreeResponse, KillTreeResponse, KillZombiesResponse,
    ListActiveResponse, ListByOriginatorResponse, PingResponse, ProcessState, RegisterResponse,
    ServiceDeleteResponse, ServiceDescribeResponse, ServiceFlushResponse, ServiceListResponse,
    ServiceLogsResponse, ServiceRestartResponse, ServiceResurrectResponse, ServiceSaveResponse,
    ServiceStartResponse, ServiceStopResponse, ShutdownResponse, SpawnDaemonResponse, StatusCode,
    StatusResponse, TrackedProcess, UnregisterResponse, ZombieReport,
};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;
use sysinfo::{Pid, ProcessRefreshKind, System};
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

#[derive(Debug)]
struct SpawnedChild {
    pid: u32,
    created_at: f64,
}

fn unix_now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        // IMPORTANT: do NOT use `raw_arg` here. running_process_core's
        // sanitized spawn rebuilds the Win32 command line from
        // `cmd.get_program()` + `cmd.get_args()` (it can't reach
        // Rust stdlib's internal `raw_arg` storage), so any
        // raw_arg-only args are LOST and cmd.exe is launched with
        // zero arguments — it exits immediately with no output and
        // never appears in `list_active`. Use regular `arg()` and
        // let Rust's CRT-escape rule + cmd's `/S` flag strip the
        // outer quotes again on the cmd side.
        let mut cmd = Command::new("cmd.exe");
        cmd.arg("/D").arg("/S").arg("/C").arg(command);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.arg("-lc").arg(command);
        cmd
    }
}

fn process_created_at(pid: u32) -> Option<f64> {
    let mut system = System::new();
    let sysinfo_pid = Pid::from_u32(pid);
    system.refresh_process_specifics(sysinfo_pid, ProcessRefreshKind::new());
    system
        .process(sysinfo_pid)
        .map(|process| process.start_time() as f64)
}

fn spawn_and_track_detached(
    command_text: &str,
    cwd: &str,
    env: &std::collections::HashMap<String, String>,
    originator: &str,
    state: &DaemonState,
) -> Result<SpawnedChild, String> {
    let mut command = shell_command(command_text);

    if !cwd.is_empty() {
        command.current_dir(cwd);
    }
    if !env.is_empty() {
        // Layer the caller-supplied env vars ON TOP of the daemon's
        // inherited env — do NOT env_clear(). On Windows, wiping the
        // env removes SystemRoot/PATH/etc. and cmd.exe (spawned via
        // `cmd /D /S /C "..."`) loses access to ping, echo redirection
        // semantics, and even its own DLL loader, exiting instantly
        // with no observable output and never appearing in
        // `list_active`.
        command.envs(env.iter());
    }
    if !originator.is_empty() {
        command.env(ORIGINATOR_ENV_VAR, originator);
    }

    // Route through `spawn_daemon` so the child gets the structurally-safe
    // sanitized handle list (no orphan inheritable handles), NUL stdio,
    // CREATE_NO_WINDOW + CREATE_NEW_PROCESS_GROUP on Windows (no console
    // popup, ignores parent's Ctrl-C), and setsid + close-extra-fds on Unix.
    let mut detached = running_process_core::spawn_daemon(&mut command)
        .map_err(|e| format!("failed to spawn detached command: {e}"))?;

    let pid = detached.id();
    let created_at = process_created_at(pid).unwrap_or_else(unix_now_seconds);
    let created_at_ms = registry::created_at_to_ms(created_at);

    let entry = TrackedEntry {
        pid,
        created_at_ms,
        kind: "subprocess".to_string(),
        command: command_text.to_string(),
        cwd: cwd.to_string(),
        originator: originator.to_string(),
        containment: "detached".to_string(),
        registered_at: unix_now_seconds(),
    };

    if let Err(e) = state.registry.register(entry) {
        let _ = detached.kill();
        let _ = detached.wait();
        return Err(format!("registry error: {e}"));
    }

    let registry = Arc::clone(&state.registry);
    std::thread::spawn(move || {
        let _ = detached.wait();
        let _ = registry.unregister_exact(pid, created_at_ms);
    });

    Ok(SpawnedChild { pid, created_at })
}

/// Handle a `SpawnDaemon` request by spawning and tracking a detached command.
pub fn handle_spawn_daemon(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let Some(ref req) = request.spawn_daemon else {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "missing spawn_daemon payload".into(),
        );
    };

    let command_text = req.command.trim();
    if command_text.is_empty() {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "command must not be empty".into(),
        );
    }

    let effective_originator = if req.originator.trim().is_empty() {
        request.client_name.clone()
    } else {
        req.originator.clone()
    };

    match spawn_and_track_detached(
        command_text,
        &req.cwd,
        &req.env,
        &effective_originator,
        state,
    ) {
        Ok(spawned) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            spawn_daemon: Some(SpawnDaemonResponse {
                pid: spawned.pid,
                created_at: spawned.created_at,
                command: command_text.to_string(),
                cwd: req.cwd.clone(),
                originator: effective_originator,
                containment: "detached".to_string(),
            }),
            ..Default::default()
        },
        Err(message) => error_response(request.id, StatusCode::Internal, message),
    }
}

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
        reports.extend(orphan_conhosts.iter().zip(conhost_results.iter()).map(
            |(z, (_pid, killed))| ZombieReport {
                pid: z.pid,
                command: z.command.clone(),
                reason: z.reason.clone(),
                killed: *killed,
            },
        ));
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

// --- service supervision (runpm) — Phase 1 stubs ---
//
// The handlers below acknowledge the new SERVICE_* request types so the
// wire protocol round-trips successfully while the real supervisor lands
// in Phase 2 of #106. Each returns StatusCode::Ok with a default-valued
// response payload — no service state is created, mutated, or persisted.

/// Phase 1 stub for `ServiceStart` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_start(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_start: Some(ServiceStartResponse::default()),
        ..Default::default()
    }
}

/// Phase 1 stub for `ServiceStop` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_stop(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_stop: Some(ServiceStopResponse::default()),
        ..Default::default()
    }
}

/// Phase 1 stub for `ServiceRestart` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_restart(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_restart: Some(ServiceRestartResponse::default()),
        ..Default::default()
    }
}

/// Phase 1 stub for `ServiceDelete` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_delete(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_delete: Some(ServiceDeleteResponse::default()),
        ..Default::default()
    }
}

/// Phase 1 stub for `ServiceList` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_list(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_list: Some(ServiceListResponse::default()),
        ..Default::default()
    }
}

/// Phase 1 stub for `ServiceDescribe` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_describe(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_describe: Some(ServiceDescribeResponse::default()),
        ..Default::default()
    }
}

/// Phase 1 stub for `ServiceLogs` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_logs(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_logs: Some(ServiceLogsResponse::default()),
        ..Default::default()
    }
}

/// Phase 1 stub for `ServiceFlush` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_flush(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_flush: Some(ServiceFlushResponse::default()),
        ..Default::default()
    }
}

/// Phase 1 stub for `ServiceSave` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_save(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_save: Some(ServiceSaveResponse::default()),
        ..Default::default()
    }
}

/// Phase 1 stub for `ServiceResurrect` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_resurrect(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_resurrect: Some(ServiceResurrectResponse::default()),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "handlers_tests.rs"]
mod tests;
