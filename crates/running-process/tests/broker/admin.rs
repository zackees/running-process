#![cfg(feature = "client")]

#[cfg(feature = "daemon")]
use std::io;
use std::io::Cursor;
#[cfg(feature = "daemon")]
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(feature = "daemon")]
use interprocess::local_socket::traits::Listener;
#[cfg(feature = "daemon")]
use interprocess::local_socket::ListenerOptions;
use prost::Message;
use serde_json::Value;

use running_process::broker::backend_handle::BackendHandle;
use running_process::broker::client::{send_admin_request, BrokerClientError};
#[cfg(feature = "daemon")]
use running_process::broker::lifecycle::CRASH_DUMP_DIR_ENV;
use running_process::broker::protocol::{
    read_frame, write_frame, AdminReply, AdminReplyKind, AdminRequest, AdminVerb, Frame, FrameKind,
    PayloadEncoding,
};
use running_process::broker::server::admin::{
    handle_admin_connection, handle_admin_frame, render_admin_reply, render_backend_health_json,
    render_config_json, render_diagnose_json, render_dump_json, render_healthz,
    render_list_instances_json, render_metrics_text, render_readyz, render_status_json,
    AdminBackend, AdminSnapshot, AdminSpawnBudget, ADMIN_SCHEMA_VERSION,
};
#[cfg(feature = "daemon")]
use running_process::broker::server::local_socket_name;
use running_process::broker::server::{
    serve_one_admin_socket, BackendKey, BackendRegistry, BrokerInstanceKey, SpawnBudgetSnapshot,
    ADMIN_PAYLOAD_PROTOCOL,
};

use crate::backend_handle_common::current_daemon;

fn snapshot() -> AdminSnapshot {
    AdminSnapshot {
        broker_instance: "shared".into(),
        broker_pid: 1234,
        generated_at_unix_ms: 1700000000000,
        uptime: std::time::Duration::from_secs(12),
        accepting_hello: true,
        connections_open: 1,
        backends: vec![AdminBackend {
            service_name: "zccache".into(),
            service_version: "1.11.20".into(),
            pid: 4321,
            backend_pipe: "rpb-v1-test-backend".into(),
            last_active_unix_ms: 1700000000000,
            state: "running".into(),
            last_hello_unix_ms: 1700000000000,
            last_error: None,
        }],
        spawn_budgets: vec![AdminSpawnBudget {
            broker_instance: "shared".into(),
            service_name: "zccache".into(),
            service_version: "1.11.20".into(),
            attempts_used: 1,
            remaining: 2,
            in_flight: false,
            retry_after_ms: None,
        }],
    }
}

fn parse_json(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap()
}

#[test]
fn status_json_uses_common_admin_envelope() {
    let value = parse_json(&render_status_json(&snapshot()));

    assert_eq!(value["schema_version"], ADMIN_SCHEMA_VERSION);
    assert_eq!(value["command"], "status");
    assert_eq!(value["broker_instance"], "shared");
    assert_eq!(value["backends"][0]["service_name"], "zccache");
}

#[test]
fn all_json_admin_verbs_include_schema_version() {
    let snapshot = snapshot();
    let outputs = [
        render_dump_json(&snapshot),
        render_list_instances_json(&snapshot),
        render_backend_health_json(&snapshot, "zccache"),
        render_config_json(&snapshot),
        render_diagnose_json(&snapshot, "bundle.tar.gz"),
    ];

    for output in outputs {
        assert_eq!(parse_json(&output)["schema_version"], ADMIN_SCHEMA_VERSION);
    }
}

#[test]
fn backend_health_filters_by_service() {
    let value = parse_json(&render_backend_health_json(&snapshot(), "clud"));

    assert_eq!(value["command"], "backend-health");
    assert_eq!(value["service_name"], "clud");
    assert_eq!(value["backends"].as_array().unwrap().len(), 0);
}

#[test]
fn healthz_and_readyz_bodies_are_stable() {
    assert_eq!(render_healthz(), "ok\n");
    assert_eq!(render_readyz(&snapshot()), "ready\n");
}

#[test]
fn metrics_text_contains_frozen_metric_names() {
    let metrics = render_metrics_text(&snapshot());

    assert!(metrics.contains("# TYPE running_process_broker_v1_connections_open gauge"));
    assert!(metrics.contains("running_process_broker_v1_connections_open 1"));
    assert!(metrics.contains("running_process_broker_v1_hello_total"));
    assert!(metrics.ends_with("# EOF\n"));
}

#[test]
fn admin_snapshot_from_registry_includes_live_backend_rows() {
    let daemon = current_daemon();
    let expected_pipe = daemon.ipc_endpoint.path.clone();
    let handle =
        BackendHandle::probe_with_service("zccache", "1.11.20", &daemon.ipc_endpoint, &daemon)
            .unwrap();
    let mut registry = BackendRegistry::new();
    registry.insert(BrokerInstanceKey::Shared, handle);

    let snapshot = AdminSnapshot::from_registry_at(
        "shared",
        1234,
        1700000000000,
        std::time::Duration::from_secs(12),
        true,
        3,
        &registry,
        &[],
    );

    assert_eq!(snapshot.backends.len(), 1);
    assert_eq!(snapshot.backends[0].service_name, "zccache");
    assert_eq!(snapshot.backends[0].service_version, "1.11.20");
    assert_eq!(snapshot.backends[0].backend_pipe, expected_pipe);
    assert_eq!(snapshot.backends[0].state, "running");
    assert_eq!(snapshot.connections_open, 3);
}

#[test]
fn dump_json_includes_spawn_budget_rows() {
    let key = BackendKey::new(BrokerInstanceKey::Shared, "zccache", "1.11.20");
    let snapshot = AdminSnapshot::from_registry_at(
        "shared",
        1234,
        1700000000000,
        std::time::Duration::from_secs(12),
        true,
        0,
        &BackendRegistry::new(),
        &[SpawnBudgetSnapshot {
            key,
            attempts_used: 3,
            remaining: 0,
            in_flight: false,
            retry_after: Some(std::time::Duration::from_millis(1500)),
        }],
    );

    let value = parse_json(&render_dump_json(&snapshot));
    let budget = &value["spawn_budgets"][0];

    assert_eq!(budget["broker_instance"], "shared");
    assert_eq!(budget["service_name"], "zccache");
    assert_eq!(budget["service_version"], "1.11.20");
    assert_eq!(budget["attempts_used"], 3);
    assert_eq!(budget["remaining"], 0);
    assert_eq!(budget["retry_after_ms"], 1500);
}

#[test]
fn config_json_includes_structured_effective_config() {
    let value = parse_json(&render_config_json(&snapshot()));
    let config = &value["values"];

    assert_eq!(config["broker"]["broker_instance"]["value"], "shared");
    assert_eq!(config["broker"]["broker_instance"]["source"], "runtime");
    assert_eq!(config["broker"]["accepting_hello"]["value"], true);
    assert_eq!(
        config["protocol"]["admin_payload_protocol"]["value"],
        "0xAD01"
    );
    assert_eq!(
        config["protocol"]["envelope_version"]["source"],
        "protocol-v1"
    );
    assert_eq!(config["limits"]["max_hello_bytes"]["value"], 64 * 1024);
    assert_eq!(
        config["spawn_budget"]["default_attempts_per_window"]["value"],
        3
    );
    assert_eq!(config["diagnostics"]["bundle_format"]["value"], "tar.gz");
    assert_eq!(config["diagnostics"]["redactions"]["value"][0], "home");
}

#[test]
fn dump_json_reuses_effective_config_model() {
    let snapshot = snapshot();
    let dump = parse_json(&render_dump_json(&snapshot));
    let config = parse_json(&render_config_json(&snapshot));

    assert_eq!(dump["effective_config"], config["values"]);
}

#[test]
fn diagnose_json_includes_deterministic_bundle_metadata() {
    let value = parse_json(&render_diagnose_json(&snapshot(), "bundle.tar.gz"));
    let bundle = &value["bundle"];

    assert_eq!(bundle["format"], "tar.gz");
    assert_eq!(bundle["mode"], "metadata-only");
    assert_eq!(bundle["created"], false);
    assert_eq!(value["files"][0], "admin/status.json");
    assert_eq!(value["redactions"][1], "secret-env");
    assert_eq!(value["redaction_policy"][2]["replacement"], "stable-hash");

    let backend_entry = bundle["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == "process/backends.json")
        .unwrap();
    assert_eq!(backend_entry["kind"], "json");
    assert_eq!(backend_entry["source"], "backend-table");
    assert_eq!(backend_entry["required"], true);
    assert_eq!(backend_entry["redacted"], true);
    assert_eq!(backend_entry["record_count"], 1);
}

#[test]
fn admin_request_dispatches_status_json_reply() {
    let request = AdminRequest {
        verb: AdminVerb::Status as i32,
        json: true,
        service_name: String::new(),
        output_path: String::new(),
    };

    let reply = render_admin_reply(&snapshot(), &request);

    assert_eq!(
        AdminReplyKind::try_from(reply.kind),
        Ok(AdminReplyKind::Json)
    );
    assert_eq!(reply.exit_code, 0);
    assert_eq!(reply.content_type, "application/json");
    let value = parse_json(&reply.body);
    assert_eq!(value["command"], "status");
    assert_eq!(value["broker_instance"], "shared");
}

#[test]
fn admin_frame_round_trips_response_metadata_and_payload() {
    let request = AdminRequest {
        verb: AdminVerb::BackendHealth as i32,
        json: true,
        service_name: "zccache".into(),
        output_path: String::new(),
    };
    let frame = Frame {
        envelope_version: 1,
        kind: FrameKind::Request as i32,
        payload_protocol: ADMIN_PAYLOAD_PROTOCOL,
        payload: request.encode_to_vec(),
        request_id: 44,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(),
        tracestate: "vendor=value".into(),
    };

    let response = handle_admin_frame(frame, &snapshot()).unwrap();
    let reply = AdminReply::decode(response.payload.as_slice()).unwrap();

    assert_eq!(response.envelope_version, 1);
    assert_eq!(FrameKind::try_from(response.kind), Ok(FrameKind::Response));
    assert_eq!(response.payload_protocol, ADMIN_PAYLOAD_PROTOCOL);
    assert_eq!(response.request_id, 44);
    assert_eq!(
        response.traceparent,
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
    );
    assert_eq!(response.tracestate, "vendor=value");
    assert_eq!(
        AdminReplyKind::try_from(reply.kind),
        Ok(AdminReplyKind::Json)
    );
    assert_eq!(parse_json(&reply.body)["command"], "backend-health");
}

#[test]
fn admin_frame_rejects_non_admin_payload_protocol() {
    let frame = Frame {
        envelope_version: 1,
        kind: FrameKind::Request as i32,
        payload_protocol: 0,
        payload: Vec::new(),
        request_id: 44,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    };

    let err = handle_admin_frame(frame, &snapshot()).unwrap_err();

    assert_eq!(
        err.to_string(),
        "admin frame payload_protocol must be 0xAD01, got 0"
    );
}

#[test]
fn handle_admin_connection_writes_admin_reply_frame() {
    let request = AdminRequest {
        verb: AdminVerb::Metrics as i32,
        json: false,
        service_name: String::new(),
        output_path: String::new(),
    };
    let frame = admin_frame(request, 77);
    let mut request_bytes = Vec::new();
    write_frame(&mut request_bytes, &frame.encode_to_vec()).unwrap();
    let request_len = request_bytes.len();
    let mut stream = Cursor::new(request_bytes);

    let returned_reply = handle_admin_connection(&mut stream, &snapshot()).unwrap();
    let response_bytes = &stream.get_ref()[request_len..];
    let mut cursor = Cursor::new(response_bytes);
    let response_frame_bytes = read_frame(&mut cursor).unwrap();
    let response_frame = Frame::decode(response_frame_bytes.as_slice()).unwrap();
    let reply = AdminReply::decode(response_frame.payload.as_slice()).unwrap();

    assert_eq!(returned_reply, reply);
    assert_eq!(
        FrameKind::try_from(response_frame.kind),
        Ok(FrameKind::Response)
    );
    assert_eq!(response_frame.payload_protocol, ADMIN_PAYLOAD_PROTOCOL);
    assert_eq!(response_frame.request_id, 77);
    assert_eq!(
        AdminReplyKind::try_from(reply.kind),
        Ok(AdminReplyKind::Openmetrics)
    );
    assert!(reply
        .body
        .contains("running_process_broker_v1_connections_open 1"));
}

#[test]
fn serve_one_admin_socket_round_trips_client_request() {
    let socket_name = unique_socket_name();
    let server_socket = socket_name.clone();
    let server = thread::spawn(move || serve_one_admin_socket(&server_socket, &snapshot()));

    let request = AdminRequest {
        verb: AdminVerb::Status as i32,
        json: true,
        service_name: String::new(),
        output_path: String::new(),
    };
    let client_reply = send_admin_request_with_retry(&socket_name, request);
    let server_reply = server.join().unwrap().unwrap();

    assert_eq!(server_reply, client_reply);
    assert_eq!(
        AdminReplyKind::try_from(client_reply.kind),
        Ok(AdminReplyKind::Json)
    );
    assert_eq!(parse_json(&client_reply.body)["command"], "status");
}

#[cfg(feature = "daemon")]
#[test]
fn broker_cli_status_queries_live_admin_socket() {
    let socket_name = unique_socket_name();
    let server = spawn_admin_socket_once(socket_name.clone());

    let output = std::process::Command::new(broker_cli())
        .args(["--socket", &socket_name, "status", "--json"])
        .output()
        .unwrap();

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let value = parse_json(&stdout(&output));
    assert_eq!(value["command"], "status");
    assert_eq!(value["broker_instance"], "shared");
    assert_eq!(value["accepting_hello"], true);
    let server_reply = server.join().unwrap().unwrap();
    assert_eq!(server_reply.exit_code, 0);
}

#[cfg(feature = "daemon")]
#[test]
fn broker_cli_status_without_socket_uses_local_snapshot() {
    let output = std::process::Command::new(broker_cli())
        .args(["status", "--json"])
        .output()
        .unwrap();

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let value = parse_json(&stdout(&output));
    assert_eq!(value["command"], "status");
    assert_eq!(value["broker_instance"], "local");
    assert_eq!(value["accepting_hello"], false);
}

#[cfg(feature = "daemon")]
#[test]
fn broker_cli_installs_configured_crash_dump_dir_before_dispatch() {
    let dir = std::env::temp_dir().join(format!(
        "rpb-v1-crash-dumps-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let output = std::process::Command::new(broker_cli())
        .env(CRASH_DUMP_DIR_ENV, &dir)
        .arg("--version")
        .output()
        .unwrap();

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(dir.is_dir(), "crash dump dir was not created: {dir:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

fn admin_frame(request: AdminRequest, request_id: u64) -> Frame {
    Frame {
        envelope_version: 1,
        kind: FrameKind::Request as i32,
        payload_protocol: ADMIN_PAYLOAD_PROTOCOL,
        payload: request.encode_to_vec(),
        request_id,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    }
}

fn send_admin_request_with_retry(socket_name: &str, request: AdminRequest) -> AdminReply {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match send_admin_request(socket_name, request.clone()) {
            Ok(reply) => return reply,
            Err(BrokerClientError::BrokerConnect(err))
                if Instant::now() < deadline && is_pending_bind_error(&err) =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => panic!("failed to send admin request: {err}"),
        }
    }
}

fn is_pending_bind_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
    )
}

#[cfg(windows)]
fn unique_socket_name() -> String {
    format!("rpb-v1-admin-{}-{}", std::process::id(), unique_suffix())
}

#[cfg(unix)]
fn unique_socket_name() -> String {
    std::env::temp_dir()
        .join(format!(
            "rpb-v1-admin-{}-{}.sock",
            std::process::id(),
            unique_suffix()
        ))
        .to_string_lossy()
        .into_owned()
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

#[cfg(feature = "daemon")]
fn spawn_admin_socket_once(socket_name: String) -> thread::JoinHandle<io::Result<AdminReply>> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = bind_test_socket(&socket_name)?;
        ready_tx.send(()).unwrap();
        let mut stream = listener.accept()?;
        let reply = handle_admin_connection(&mut stream, &snapshot())
            .map_err(|err| io::Error::other(err.to_string()))?;
        cleanup_test_socket(&socket_name);
        Ok(reply)
    });
    ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    handle
}

#[cfg(feature = "daemon")]
fn bind_test_socket(socket_name: &str) -> io::Result<interprocess::local_socket::Listener> {
    prepare_test_socket(socket_name)?;
    let name = local_socket_name(socket_name)?;
    ListenerOptions::new().name(name).create_sync()
}

#[cfg(feature = "daemon")]
fn prepare_test_socket(socket_name: &str) -> io::Result<()> {
    #[cfg(unix)]
    {
        let path = std::path::Path::new(socket_name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(path);
    }

    #[cfg(windows)]
    let _ = socket_name;

    Ok(())
}

#[cfg(feature = "daemon")]
fn cleanup_test_socket(socket_name: &str) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(socket_name);
    }

    #[cfg(windows)]
    let _ = socket_name;
}

#[cfg(feature = "daemon")]
fn broker_cli() -> &'static str {
    env!("CARGO_BIN_EXE_running-process-broker-v1")
}

#[cfg(feature = "daemon")]
fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[cfg(feature = "daemon")]
fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
