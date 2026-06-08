//! Admin verb rendering for the v1 broker.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::broker::server::{
    BackendRegistry, SpawnBudgetSnapshot,
};
use crate::broker::server::metrics::{MetricKind, BROKER_METRICS};

/// Frozen admin JSON schema version.
pub const ADMIN_SCHEMA_VERSION: u32 = 1;

/// Snapshot consumed by admin verb renderers.
#[derive(Clone, Debug)]
pub struct AdminSnapshot {
    /// Broker instance identifier.
    pub broker_instance: String,
    /// Broker process id.
    pub broker_pid: u32,
    /// Snapshot generation timestamp.
    pub generated_at_unix_ms: u64,
    /// Time since broker start.
    pub uptime: Duration,
    /// Whether new Hello requests are accepted.
    pub accepting_hello: bool,
    /// Open control-plane connections.
    pub connections_open: u64,
    /// Known backend rows.
    pub backends: Vec<AdminBackend>,
    /// Known spawn budget rows.
    pub spawn_budgets: Vec<AdminSpawnBudget>,
}

impl AdminSnapshot {
    /// Local process snapshot used until pipe-backed admin transport lands.
    pub fn local_not_serving() -> Self {
        Self {
            broker_instance: "local".into(),
            broker_pid: std::process::id(),
            generated_at_unix_ms: unix_now_ms(),
            uptime: Duration::ZERO,
            accepting_hello: false,
            connections_open: 0,
            backends: Vec::new(),
            spawn_budgets: Vec::new(),
        }
    }

    /// Build a live snapshot from broker state.
    pub fn from_registry(
        broker_instance: impl Into<String>,
        uptime: Duration,
        accepting_hello: bool,
        connections_open: u64,
        registry: &BackendRegistry,
        spawn_budgets: &[SpawnBudgetSnapshot],
    ) -> Self {
        Self::from_registry_at(
            broker_instance,
            std::process::id(),
            unix_now_ms(),
            uptime,
            accepting_hello,
            connections_open,
            registry,
            spawn_budgets,
        )
    }

    /// Testable variant of [`Self::from_registry`] with deterministic metadata.
    #[allow(clippy::too_many_arguments)]
    pub fn from_registry_at(
        broker_instance: impl Into<String>,
        broker_pid: u32,
        generated_at_unix_ms: u64,
        uptime: Duration,
        accepting_hello: bool,
        connections_open: u64,
        registry: &BackendRegistry,
        spawn_budgets: &[SpawnBudgetSnapshot],
    ) -> Self {
        Self {
            broker_instance: broker_instance.into(),
            broker_pid,
            generated_at_unix_ms,
            uptime,
            accepting_hello,
            connections_open,
            backends: registry
                .iter()
                .map(|(_key, handle)| AdminBackend {
                    service_name: handle.service_name.clone(),
                    service_version: handle.service_version.clone(),
                    pid: handle.daemon_process.pid,
                    backend_pipe: handle.daemon_process.ipc_endpoint.path.clone(),
                    last_active_unix_ms: handle.daemon_process.started_at_unix_ms,
                    state: if handle.is_alive() {
                        "running".into()
                    } else {
                        "stale".into()
                    },
                    last_hello_unix_ms: 0,
                    last_error: None,
                })
                .collect(),
            spawn_budgets: spawn_budgets
                .iter()
                .map(AdminSpawnBudget::from_snapshot)
                .collect(),
        }
    }
}

/// Backend row used in admin output.
#[derive(Clone, Debug)]
pub struct AdminBackend {
    /// Logical service name.
    pub service_name: String,
    /// Service version.
    pub service_version: String,
    /// Backend process id.
    pub pid: u32,
    /// Backend pipe/socket path.
    pub backend_pipe: String,
    /// Last activity timestamp.
    pub last_active_unix_ms: u64,
    /// Human-readable state.
    pub state: String,
    /// Last Hello timestamp.
    pub last_hello_unix_ms: u64,
    /// Last backend error.
    pub last_error: Option<String>,
}

/// Spawn budget row used in admin output.
#[derive(Clone, Debug)]
pub struct AdminSpawnBudget {
    /// Broker instance identifier.
    pub broker_instance: String,
    /// Logical service name.
    pub service_name: String,
    /// Service version.
    pub service_version: String,
    /// Attempts used in the active window.
    pub attempts_used: u32,
    /// Attempts remaining in the active window.
    pub remaining: u32,
    /// Whether a spawn is currently in flight.
    pub in_flight: bool,
    /// Retry-after hint when exhausted.
    pub retry_after_ms: Option<u64>,
}

impl AdminSpawnBudget {
    fn from_snapshot(snapshot: &SpawnBudgetSnapshot) -> Self {
        Self {
            broker_instance: snapshot.key.instance.id(),
            service_name: snapshot.key.service_name.clone(),
            service_version: snapshot.key.service_version.clone(),
            attempts_used: snapshot.attempts_used,
            remaining: snapshot.remaining,
            in_flight: snapshot.in_flight,
            retry_after_ms: snapshot
                .retry_after
                .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)),
        }
    }
}

/// Render `status --json`.
pub fn render_status_json(snapshot: &AdminSnapshot) -> String {
    json!({
        "schema_version": ADMIN_SCHEMA_VERSION,
        "command": "status",
        "generated_at_unix_ms": snapshot.generated_at_unix_ms,
        "broker_instance": snapshot.broker_instance,
        "broker_pid": snapshot.broker_pid,
        "uptime_seconds": snapshot.uptime.as_secs_f64(),
        "accepting_hello": snapshot.accepting_hello,
        "connections_open": snapshot.connections_open,
        "backends": snapshot.backends.iter().map(|backend| {
            json!({
                "service_name": backend.service_name,
                "service_version": backend.service_version,
                "pid": backend.pid,
                "backend_pipe": backend.backend_pipe,
                "last_active_unix_ms": backend.last_active_unix_ms,
                "state": backend.state,
            })
        }).collect::<Vec<_>>(),
    })
    .to_string()
}

/// Render `dump --json`.
pub fn render_dump_json(snapshot: &AdminSnapshot) -> String {
    json!({
        "schema_version": ADMIN_SCHEMA_VERSION,
        "command": "dump",
        "generated_at_unix_ms": snapshot.generated_at_unix_ms,
        "broker_instance": snapshot.broker_instance,
        "effective_config": {},
        "backend_table": snapshot.backends.iter().map(|backend| {
            json!({
                "service_name": backend.service_name,
                "service_version": backend.service_version,
                "pid": backend.pid,
                "backend_pipe": backend.backend_pipe,
                "state": backend.state,
            })
        }).collect::<Vec<_>>(),
        "spawn_budgets": snapshot.spawn_budgets.iter().map(|budget| {
            json!({
                "broker_instance": budget.broker_instance,
                "service_name": budget.service_name,
                "service_version": budget.service_version,
                "attempts_used": budget.attempts_used,
                "remaining": budget.remaining,
                "in_flight": budget.in_flight,
                "retry_after_ms": budget.retry_after_ms,
            })
        }).collect::<Vec<_>>(),
        "recent_lifecycle_events": [],
    })
    .to_string()
}

/// Render `list-instances --json`.
pub fn render_list_instances_json(snapshot: &AdminSnapshot) -> String {
    json!({
        "schema_version": ADMIN_SCHEMA_VERSION,
        "command": "list-instances",
        "generated_at_unix_ms": snapshot.generated_at_unix_ms,
        "instances": [{
            "broker_instance": snapshot.broker_instance,
            "pipe": "",
            "pid": snapshot.broker_pid,
            "state": if snapshot.accepting_hello { "running" } else { "not-serving" },
        }],
    })
    .to_string()
}

/// Render `backend-health <service> --json`.
pub fn render_backend_health_json(snapshot: &AdminSnapshot, service_name: &str) -> String {
    json!({
        "schema_version": ADMIN_SCHEMA_VERSION,
        "command": "backend-health",
        "generated_at_unix_ms": snapshot.generated_at_unix_ms,
        "service_name": service_name,
        "backends": snapshot.backends.iter()
            .filter(|backend| backend.service_name == service_name)
            .map(|backend| {
                json!({
                    "service_version": backend.service_version,
                    "pid": backend.pid,
                    "state": backend.state,
                    "last_hello_unix_ms": backend.last_hello_unix_ms,
                    "last_error": backend.last_error,
                })
            })
            .collect::<Vec<_>>(),
    })
    .to_string()
}

/// Render `config --effective --json`.
pub fn render_config_json(snapshot: &AdminSnapshot) -> String {
    json!({
        "schema_version": ADMIN_SCHEMA_VERSION,
        "command": "config",
        "generated_at_unix_ms": snapshot.generated_at_unix_ms,
        "values": {},
    })
    .to_string()
}

/// Render `diagnose --output <path>` summary JSON.
pub fn render_diagnose_json(snapshot: &AdminSnapshot, output: &str) -> String {
    json!({
        "schema_version": ADMIN_SCHEMA_VERSION,
        "command": "diagnose",
        "generated_at_unix_ms": snapshot.generated_at_unix_ms,
        "output": output,
        "files": [],
        "redactions": ["home", "secret-env", "acl-identities"],
    })
    .to_string()
}

/// Render OpenMetrics text.
pub fn render_metrics_text(snapshot: &AdminSnapshot) -> String {
    let mut out = String::new();
    for metric in BROKER_METRICS {
        out.push_str("# TYPE ");
        out.push_str(metric.name);
        out.push(' ');
        out.push_str(metric_kind_name(metric.kind));
        out.push('\n');
        if metric.labels.is_empty() {
            out.push_str(metric.name);
            out.push(' ');
            out.push_str(&metric_value(metric.name, snapshot));
            out.push('\n');
        }
    }
    out.push_str("# EOF\n");
    out
}

/// Health endpoint body.
pub fn render_healthz() -> &'static str {
    "ok\n"
}

/// Readiness endpoint body.
pub fn render_readyz(snapshot: &AdminSnapshot) -> &'static str {
    if snapshot.accepting_hello {
        "ready\n"
    } else {
        "not ready\n"
    }
}

fn metric_kind_name(kind: MetricKind) -> &'static str {
    match kind {
        MetricKind::Counter => "counter",
        MetricKind::Gauge => "gauge",
        MetricKind::Histogram => "histogram",
    }
}

fn metric_value(name: &str, snapshot: &AdminSnapshot) -> String {
    match name {
        "running_process_broker_v1_connections_open" => snapshot.connections_open.to_string(),
        "running_process_broker_v1_fd_usage_ratio" => "0".into(),
        "running_process_broker_v1_uptime_seconds" => snapshot.uptime.as_secs().to_string(),
        _ => "0".into(),
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
