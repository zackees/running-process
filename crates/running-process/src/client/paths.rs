//! Shared path computation for daemon socket, PID file, database, and shadow directory.
//!
//! Both the server and client modules use these functions to agree on where
//! the daemon listens and where auxiliary files are stored.

use std::path::PathBuf;

/// Directory name this product owns beneath each host location.
const PRODUCT: &str = "running-process";

/// Returns the local socket name the daemon listens on.
///
/// - **Linux/macOS**: `$XDG_RUNTIME_DIR/running-process/daemon{-hash}.sock`
///   (fallback: `/tmp/running-process-{uid}/daemon{-hash}.sock`)
/// - **Windows**: `\\.\pipe\running-process-daemon-{username}{-hash}`
///
/// The returned display path must be passed through
/// [`crate::broker::server::singleton_bind::wrap_socket_name`] before use.
/// That shared boundary prevents Windows pipe paths from acquiring the
/// namespace prefix more than once.
pub fn socket_path(scope_hash: Option<&str>) -> String {
    // This is a product-owned, dedicated leaf, so the caller is allowed to
    // repair legacy permissions before the platform bind verifies it. A host
    // whose sockets live in a kernel namespace rather than a directory has
    // nothing there to repair.
    if crate::platform::ipc::endpoint_is_filesystem_backed() {
        let _ = crate::broker::secure_dir::ensure_private_dir(&runtime_dir());
    }
    socket_path_view(scope_hash)
}

/// Read-only variant of [`socket_path`]: derives the same endpoint string
/// without creating any directory. Used by read-only inspectors (#391).
pub fn socket_path_view(scope_hash: Option<&str>) -> String {
    let suffix = match scope_hash {
        Some(h) => format!("-{h}"),
        None => String::new(),
    };

    if crate::platform::ipc::endpoint_is_filesystem_backed() {
        return format!("{}/daemon{suffix}.sock", runtime_dir().display());
    }
    let username = crate::env_vars::USERNAME
        .text()
        .unwrap_or_else(|| "unknown".into());
    format!(r"\\.\pipe\running-process-daemon-{username}{suffix}")
}

/// Build an opaque local IPC endpoint from the path returned by [`socket_path`].
///
/// This must use the same name-type dispatch as the server so that client
/// and server agree on the actual IPC endpoint.
pub fn make_socket_endpoint(path: &str) -> std::io::Result<crate::platform::ipc::Endpoint> {
    crate::platform::ipc::Endpoint::new(path)
}

/// Returns the path to the daemon PID file.
///
/// - **Linux/macOS**: same directory as the socket, with `.pid` extension.
/// - **Windows**: `%LOCALAPPDATA%\running-process\daemon{-hash}.pid`
pub fn pid_file_path(scope_hash: Option<&str>) -> PathBuf {
    let path = pid_file_path_view(scope_hash);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    path
}

/// Read-only variant of [`pid_file_path`]: derives the same path without
/// creating any directory. Used by read-only inspectors (#391).
pub fn pid_file_path_view(scope_hash: Option<&str>) -> PathBuf {
    let suffix = match scope_hash {
        Some(h) => format!("-{h}"),
        None => String::new(),
    };

    runtime_dir().join(format!("daemon{suffix}.pid"))
}

/// Returns the path to the daemon SQLite database.
///
/// - **Linux/macOS**: `$XDG_STATE_HOME/running-process/tracked-pids{-hash}.sqlite3`
///   (fallback: `~/.local/state/running-process/tracked-pids{-hash}.sqlite3`)
/// - **Windows**: `%LOCALAPPDATA%\running-process\tracked-pids{-hash}.sqlite3`
pub fn db_path(scope_hash: Option<&str>) -> PathBuf {
    let path = db_path_view(scope_hash);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    path
}

/// Read-only variant of [`db_path`]: derives the same path without creating
/// any directory. Used by read-only inspectors (#391).
pub fn db_path_view(scope_hash: Option<&str>) -> PathBuf {
    let suffix = match scope_hash {
        Some(h) => format!("-{h}"),
        None => String::new(),
    };
    data_dir().join(format!("tracked-pids{suffix}.sqlite3"))
}

/// Returns the shadow directory used for ephemeral run data.
///
/// - **Windows**: `%LOCALAPPDATA%\running-process\run\`
/// - **Linux**: `$XDG_RUNTIME_DIR/running-process/run/`
/// - **macOS**: `$HOME/Library/Caches/running-process/run/`
pub fn shadow_dir() -> PathBuf {
    let dir = shadow_dir_view();
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Read-only variant of [`shadow_dir`]: derives the same path without
/// creating any directory. Used by read-only inspectors (#391).
pub fn shadow_dir_view() -> PathBuf {
    crate::platform::fs::user_run_data_root(PRODUCT).join("run")
}

/// Returns the daemon data directory (where the SQLite tracking database
/// lives) WITHOUT creating it. Read-only callers (doctor, status probes)
/// use this; [`db_path`] keeps its create-on-derive behavior.
pub fn data_dir() -> PathBuf {
    crate::platform::fs::user_state_dir(PRODUCT)
}

/// Where this host keeps our ephemeral runtime artifacts.
fn runtime_dir() -> PathBuf {
    crate::platform::fs::user_runtime_dir(PRODUCT)
}
