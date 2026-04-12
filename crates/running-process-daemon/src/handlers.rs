//! Request handlers for the daemon's IPC protocol.
//!
//! Each handler receives a [`DaemonRequest`] and a shared [`DaemonState`]
//! reference, returning a fully-constructed [`DaemonResponse`].

use running_process_proto::daemon::{
    DaemonRequest, DaemonResponse, PingResponse, ShutdownResponse, StatusCode, StatusResponse,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::watch;

use crate::registry::Registry;

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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use running_process_proto::daemon::{PingRequest, RequestType, ShutdownRequest, StatusRequest};

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
}
