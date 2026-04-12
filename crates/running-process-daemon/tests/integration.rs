//! Integration tests for the running-process daemon.
//!
//! Each test starts a `DaemonServer` in a background tokio task using a
//! unique socket path derived from `line!()`, exercises the server via
//! `DaemonClient`, and shuts down cleanly afterwards.
//!
//! All tests use `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`
//! because the `DaemonClient` performs blocking synchronous I/O.  A single-
//! threaded runtime would deadlock (the blocking client call would prevent
//! the server task from making progress on the same thread).

use running_process_daemon::client::DaemonClient;
use running_process_daemon::paths;
use running_process_daemon::server::DaemonServer;
use running_process_proto::daemon::{DaemonRequest, StatusCode};

/// Build a unique scope string for each test to avoid socket conflicts.
macro_rules! test_scope {
    () => {
        format!("integ-{}", line!())
    };
}

/// Helper: start a `DaemonServer` in a background task, returning the
/// join handle and the socket path it is listening on.
fn start_server(scope: &str) -> (tokio::task::JoinHandle<()>, String) {
    let socket = paths::socket_path(Some(scope));
    let db = paths::db_path(Some(scope))
        .to_string_lossy()
        .into_owned();

    let server = DaemonServer::new(
        socket.clone(),
        db,
        "test".to_string(),
        scope.to_string(),
        std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    );

    let handle = tokio::spawn(async move {
        server.run().await.expect("server.run() failed");
    });

    (handle, socket)
}

// ---------------------------------------------------------------------------
// Test 1: happy-path roundtrip
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_start_ping_status_shutdown_roundtrip() {
    let scope = test_scope!();
    let (server_handle, socket) = start_server(&scope);

    // Give the server a moment to bind the socket.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Run blocking client calls on a dedicated thread.
    let result = tokio::task::spawn_blocking(move || {
        let mut client =
            DaemonClient::connect_to(&socket).expect("failed to connect to daemon");

        // --- Ping ---
        let ping_resp = client.ping().expect("ping failed");
        assert_eq!(ping_resp.code, StatusCode::Ok as i32, "ping code should be OK");
        let ping_payload = ping_resp.ping.expect("ping response should have ping payload");
        assert!(
            ping_payload.server_time_ms > 0,
            "server_time_ms should be positive"
        );

        // --- Status ---
        let status_resp = client.status().expect("status failed");
        assert_eq!(
            status_resp.code,
            StatusCode::Ok as i32,
            "status code should be OK"
        );
        let status_payload = status_resp
            .status
            .expect("status response should have status payload");
        assert!(
            !status_payload.version.is_empty(),
            "version should be non-empty"
        );
        assert!(
            status_payload.uptime_seconds < 60,
            "uptime should be small in a fresh test server"
        );

        // --- Shutdown ---
        let shutdown_resp = client
            .shutdown(true, 5.0)
            .expect("shutdown failed");
        assert_eq!(
            shutdown_resp.code,
            StatusCode::Ok as i32,
            "shutdown code should be OK"
        );
    })
    .await;
    result.expect("client task panicked");

    // The server task should exit cleanly after shutdown.
    tokio::time::timeout(std::time::Duration::from_secs(5), server_handle)
        .await
        .expect("server did not stop within 5 seconds")
        .expect("server task panicked");
}

// ---------------------------------------------------------------------------
// Test 2: unknown request type
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_unknown_request_type_returns_unknown_request() {
    let scope = test_scope!();
    let (server_handle, socket) = start_server(&scope);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let result = tokio::task::spawn_blocking(move || {
        let mut client =
            DaemonClient::connect_to(&socket).expect("failed to connect to daemon");

        // Send a request with a bogus type value (999).
        let bad_request = DaemonRequest {
            id: 1,
            r#type: 999,
            protocol_version: 1,
            client_name: "test-client".to_string(),
            ..Default::default()
        };
        let resp = client
            .send_request(bad_request)
            .expect("send_request failed");

        assert_eq!(
            resp.code,
            StatusCode::UnknownRequest as i32,
            "code should be UNKNOWN_REQUEST for bogus type"
        );
        assert!(
            resp.message.contains("unknown request type"),
            "message should mention unknown type, got: {}",
            resp.message
        );

        // Clean up.
        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task panicked");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}

// ---------------------------------------------------------------------------
// Test 3: multiple pings
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_multiple_pings() {
    let scope = test_scope!();
    let (server_handle, socket) = start_server(&scope);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let result = tokio::task::spawn_blocking(move || {
        let mut client =
            DaemonClient::connect_to(&socket).expect("failed to connect to daemon");

        for i in 0..10 {
            let resp = client
                .ping()
                .unwrap_or_else(|e| panic!("ping {i} failed: {e}"));
            assert_eq!(
                resp.code,
                StatusCode::Ok as i32,
                "ping {i} should return OK"
            );
            assert!(resp.ping.is_some(), "ping {i} should have payload");
        }

        // Clean up.
        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task panicked");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}

// ---------------------------------------------------------------------------
// Test 4: status shows active connections
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_status_shows_active_connections() {
    let scope = test_scope!();
    let (server_handle, socket) = start_server(&scope);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let socket_clone = socket.clone();
    let result = tokio::task::spawn_blocking(move || {
        // Connect TWO clients.  On Windows named pipes, the server must loop
        // back to accept() before a second client can connect, so we add a
        // brief pause between connections.
        let mut client1 =
            DaemonClient::connect_to(&socket_clone).expect("failed to connect client 1");

        // Send a ping on client 1 so the server processes its accept and
        // loops back to listen for the next connection.
        let _ = client1.ping().expect("initial ping on client 1 failed");
        std::thread::sleep(std::time::Duration::from_millis(200));

        let _client2 =
            DaemonClient::connect_to(&socket_clone).expect("failed to connect client 2");

        // Give the server a moment to accept the second connection.
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Send a status request from client 1.
        let resp = client1.status().expect("status failed");
        assert_eq!(resp.code, StatusCode::Ok as i32);

        let status = resp.status.expect("status payload missing");
        // Both connections should be active. The server increments
        // active_connections on accept, before any frames are read.
        assert!(
            status.active_connections >= 2,
            "expected at least 2 active connections, got {}",
            status.active_connections
        );

        // Clean up.
        let _ = client1.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task panicked");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}
