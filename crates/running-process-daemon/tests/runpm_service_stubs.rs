//! Phase 1 integration tests for the runpm `SERVICE_*` daemon RPC stubs (#106).
//!
//! Each new request type (`ServiceStart` ... `ServiceResurrect`, enum values
//! 50-59) currently dispatches to a stub handler that responds with
//! `StatusCode::Ok` and a default-valued payload. These tests exercise every
//! wrapper on `DaemonClient` to verify the full proto round-trip — request
//! type dispatch, handler invocation, and response payload presence.
//!
//! All 10 RPCs are exercised against a single `DaemonServer` instance to
//! minimize fixture setup cost; if any individual RPC needs richer coverage
//! later, it can be split out into a dedicated test.
//!
//! Uses `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]` because
//! `DaemonClient` performs blocking synchronous I/O — a single-threaded
//! runtime would deadlock the server task.

use running_process_daemon::client::DaemonClient;
use running_process_daemon::paths;
use running_process_daemon::server::DaemonServer;
use running_process_proto::daemon::{ServiceConfig, StatusCode};

/// Build a unique scope string for each test to avoid socket conflicts.
macro_rules! test_scope {
    () => {
        format!("runpm-stubs-{}", line!())
    };
}

/// Helper: start a `DaemonServer` in a background task, returning the join
/// handle and the socket path it is listening on.  Mirrors the helper in
/// `tests/integration.rs` (kept local — that helper is private to its file).
fn start_server(scope: &str) -> (tokio::task::JoinHandle<()>, String) {
    let socket = paths::socket_path(Some(scope));
    let db = paths::db_path(Some(scope)).to_string_lossy().into_owned();

    let server = DaemonServer::new(
        socket.clone(),
        db,
        "test".to_string(),
        scope.to_string(),
        std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    )
    .expect("failed to create DaemonServer");

    let handle = tokio::spawn(async move {
        server.run().await.expect("server.run() failed");
    });

    (handle, socket)
}

// ---------------------------------------------------------------------------
// Combined Phase 1 stub roundtrip — exercises all 10 SERVICE_* RPCs against
// a single server/client to keep fixture cost low.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_all_service_stubs_return_ok() {
    let scope = test_scope!();
    let (server_handle, socket) = start_server(&scope);

    // Give the server a moment to bind the socket.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket).expect("failed to connect to daemon");

        // --- ServiceStart -----------------------------------------------
        let cfg = ServiceConfig {
            name: "test-svc".into(),
            cmd: vec!["echo".into(), "hi".into()],
            cwd: ".".into(),
            env: Default::default(),
            autorestart: false,
            max_restarts: 0,
            restart_delay_ms: 0,
            kill_timeout_ms: 0,
            min_uptime_ms: 0,
        };
        let resp = client.service_start(cfg).expect("service_start failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "service_start should return OK"
        );
        assert!(
            resp.service_start.is_some(),
            "service_start response should carry a service_start payload"
        );

        // --- ServiceStop ------------------------------------------------
        let resp = client
            .service_stop("test-svc")
            .expect("service_stop failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "service_stop should return OK"
        );
        assert!(
            resp.service_stop.is_some(),
            "service_stop response should carry a service_stop payload"
        );

        // --- ServiceRestart --------------------------------------------
        let resp = client
            .service_restart("test-svc")
            .expect("service_restart failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "service_restart should return OK"
        );
        assert!(
            resp.service_restart.is_some(),
            "service_restart response should carry a service_restart payload"
        );

        // --- ServiceDelete ---------------------------------------------
        let resp = client
            .service_delete("test-svc")
            .expect("service_delete failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "service_delete should return OK"
        );
        assert!(
            resp.service_delete.is_some(),
            "service_delete response should carry a service_delete payload"
        );

        // --- ServiceList -----------------------------------------------
        let resp = client.service_list().expect("service_list failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "service_list should return OK"
        );
        assert!(
            resp.service_list.is_some(),
            "service_list response should carry a service_list payload"
        );

        // --- ServiceDescribe -------------------------------------------
        let resp = client
            .service_describe("test-svc")
            .expect("service_describe failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "service_describe should return OK"
        );
        assert!(
            resp.service_describe.is_some(),
            "service_describe response should carry a service_describe payload"
        );

        // --- ServiceLogs -----------------------------------------------
        let resp = client
            .service_logs("test-svc", 100, false)
            .expect("service_logs failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "service_logs should return OK"
        );
        assert!(
            resp.service_logs.is_some(),
            "service_logs response should carry a service_logs payload"
        );

        // --- ServiceFlush ----------------------------------------------
        let resp = client
            .service_flush("test-svc")
            .expect("service_flush failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "service_flush should return OK"
        );
        assert!(
            resp.service_flush.is_some(),
            "service_flush response should carry a service_flush payload"
        );

        // --- ServiceSave -----------------------------------------------
        let resp = client.service_save().expect("service_save failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "service_save should return OK"
        );
        assert!(
            resp.service_save.is_some(),
            "service_save response should carry a service_save payload"
        );

        // --- ServiceResurrect ------------------------------------------
        let resp = client
            .service_resurrect()
            .expect("service_resurrect failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "service_resurrect should return OK"
        );
        assert!(
            resp.service_resurrect.is_some(),
            "service_resurrect response should carry a service_resurrect payload"
        );

        // --- Shutdown ---------------------------------------------------
        let shutdown_resp = client.shutdown(true, 5.0).expect("shutdown failed");
        assert_eq!(
            shutdown_resp.code,
            StatusCode::Ok as i32,
            "shutdown should return OK"
        );
    })
    .await;
    result.expect("client task panicked");

    // The server should exit cleanly after the shutdown RPC.
    tokio::time::timeout(std::time::Duration::from_secs(5), server_handle)
        .await
        .expect("server did not stop within 5 seconds")
        .expect("server task panicked");
}
