//! `SpawnDaemon` handler and helpers for spawning + tracking detached
//! commands.

use std::process::Command;
use std::sync::Arc;

use crate::proto::daemon::{
    DaemonRequest, DaemonResponse, KeyValue, SpawnDaemonResponse, StatusCode,
};
use crate::ORIGINATOR_ENV_VAR;
use sysinfo::{Pid, ProcessRefreshKind, System};

use crate::daemon::registry::{self, TrackedEntry};

use super::util::{error_response, unix_now_seconds};
use super::DaemonState;

#[derive(Debug)]
struct SpawnedChild {
    pid: u32,
    created_at: f64,
}

fn shell_command(command: &str) -> Command {
    running_process_platform_internal::platform::process::shell_command(command)
}

fn process_created_at(pid: u32) -> Option<f64> {
    let mut system = System::new();
    let sysinfo_pid = Pid::from_u32(pid);
    system.refresh_process_specifics(sysinfo_pid, ProcessRefreshKind::new());
    system
        .process(sysinfo_pid)
        .map(|process| process.start_time() as f64)
}

/// Normalize the caller-supplied env list into a deterministic
/// `(key, value)` sequence ready for `Command::envs`.
///
/// On Windows, env var names are case-insensitive at the kernel level but
/// Rust's `Command::env` collapses duplicates via a case-insensitive
/// `EnvKey` with **last-write-wins** semantics. If a caller passes both
/// `("PATH", inherited)` and `("Path", override)` and we hand them to
/// `Command::envs` in iteration order, whichever was inserted last wins —
/// and HashMap / protobuf-map iteration order would race that.
///
/// We dedup case-insensitively here, preserving the LAST entry per
/// case-folded key, so the caller's intended override always wins
/// regardless of upstream ordering.
fn canonical_env_pairs(env: &[KeyValue]) -> Vec<(String, String)> {
    let pairs = env
        .iter()
        .map(|kv| (kv.key.clone(), kv.value.clone()))
        .collect();
    running_process_platform_internal::platform::process::canonical_environment_pairs(pairs)
}

fn spawn_and_track_detached(
    command_text: &str,
    cwd: &str,
    env: &[KeyValue],
    environment_policy: crate::EnvironmentPolicy,
    originator: &str,
    state: &DaemonState,
) -> Result<SpawnedChild, String> {
    let mut command = shell_command(command_text);

    if !cwd.is_empty() {
        command.current_dir(cwd);
    }
    // The resolved base policy is applied by the centralized spawn boundary;
    // these ordered entries are explicit client overrides applied afterward.
    if !env.is_empty() {
        command.envs(canonical_env_pairs(env));
    }
    if !originator.is_empty() {
        command.env(ORIGINATOR_ENV_VAR, originator);
    }

    // Route through the environment-policy spawn boundary so the child gets the
    // structurally-safe sanitized handle list (no orphan inheritable
    // handles), NUL stdio, CREATE_NO_WINDOW + CREATE_NEW_PROCESS_GROUP
    // on Windows (no console popup, ignores parent's Ctrl-C), and setsid
    // + close-extra-fds on Unix. The policy is resolved before reaching the
    // platform spawn path, including the manual CreateProcessW path.
    let mut detached = crate::spawn_daemon_with_env_policy(&mut command, environment_policy)
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
    let environment_policy = match crate::EnvironmentPolicy::from_wire(
        req.environment_policy,
        req.clear_inherited_env,
    ) {
        Ok(policy) => policy,
        Err(message) => {
            return error_response(request.id, StatusCode::InvalidArgument, message.into())
        }
    };

    match spawn_and_track_detached(
        command_text,
        &req.cwd,
        &req.env,
        environment_policy,
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
