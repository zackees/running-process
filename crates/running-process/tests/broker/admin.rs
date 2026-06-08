#![cfg(feature = "client")]

use serde_json::Value;

use running_process::broker::backend_handle::BackendHandle;
use running_process::broker::server::admin::{
    render_backend_health_json, render_config_json, render_diagnose_json, render_dump_json,
    render_healthz, render_list_instances_json, render_metrics_text, render_readyz,
    render_status_json, AdminBackend, AdminSnapshot, AdminSpawnBudget, ADMIN_SCHEMA_VERSION,
};
use running_process::broker::server::{
    BackendKey, BackendRegistry, BrokerInstanceKey, SpawnBudgetSnapshot,
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
