//! Admin verb rendering for the v1 broker.

use std::io::{self, Read, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use interprocess::local_socket::traits::Listener;
use prost::Message;
use serde_json::json;

use crate::broker::protocol::{
    read_frame, write_frame, AdminReply, AdminReplyKind, AdminRequest, AdminVerb, Frame,
    FrameKind, FramingError, PayloadEncoding,
};

use super::backend_registry::BackendRegistry;
use super::connection::{bind_local_socket, BrokerConnectionError, LocalSocketCleanup};
use crate::broker::server::metrics::{MetricKind, BROKER_METRICS};
use super::spawn_coordinator::SpawnBudgetSnapshot;

/// Frozen admin JSON schema version.
pub const ADMIN_SCHEMA_VERSION: u32 = 1;
/// Payload protocol value for v1 admin request/reply frames.
pub const ADMIN_PAYLOAD_PROTOCOL: u32 = 0xAD01;

const PROTOCOL_VERSION: u32 = 1;

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

/// Render one typed admin request into a typed admin reply.
pub fn render_admin_reply(snapshot: &AdminSnapshot, request: &AdminRequest) -> AdminReply {
    match AdminVerb::try_from(request.verb) {
        Ok(AdminVerb::Status) => {
            if request.json {
                json_reply(render_status_json(snapshot))
            } else {
                text_reply(
                    format!(
                        "broker_instance: {}\naccepting_hello: {}\n",
                        snapshot.broker_instance, snapshot.accepting_hello
                    ),
                    0,
                )
            }
        }
        Ok(AdminVerb::Dump) => json_reply(render_dump_json(snapshot)),
        Ok(AdminVerb::ListInstances) => json_reply(render_list_instances_json(snapshot)),
        Ok(AdminVerb::Healthz) => text_reply(render_healthz(), 0),
        Ok(AdminVerb::Readyz) => {
            let exit_code = if snapshot.accepting_hello { 0 } else { 1 };
            text_reply(render_readyz(snapshot), exit_code)
        }
        Ok(AdminVerb::BackendHealth) => {
            let service_name = if request.service_name.is_empty() {
                "unknown"
            } else {
                &request.service_name
            };
            json_reply(render_backend_health_json(snapshot, service_name))
        }
        Ok(AdminVerb::Config) => json_reply(render_config_json(snapshot)),
        Ok(AdminVerb::Diagnose) => {
            let output = if request.output_path.is_empty() {
                "bundle.tar.gz"
            } else {
                &request.output_path
            };
            json_reply(render_diagnose_json(snapshot, output))
        }
        Ok(AdminVerb::Metrics) => AdminReply {
            kind: AdminReplyKind::Openmetrics as i32,
            body: render_metrics_text(snapshot),
            exit_code: 0,
            content_type: "application/openmetrics-text".into(),
        },
        Ok(AdminVerb::Unspecified) | Err(_) => {
            text_reply("unsupported admin verb\n", 2)
        }
    }
}

/// Handle one decoded admin frame and return a response frame.
pub fn handle_admin_frame(
    frame: Frame,
    snapshot: &AdminSnapshot,
) -> Result<Frame, AdminFrameError> {
    if frame.envelope_version != PROTOCOL_VERSION {
        return Err(AdminFrameError::UnsupportedEnvelopeVersion(
            frame.envelope_version,
        ));
    }
    if FrameKind::try_from(frame.kind) != Ok(FrameKind::Request) {
        return Err(AdminFrameError::UnexpectedKind(frame.kind));
    }
    if frame.payload_protocol != ADMIN_PAYLOAD_PROTOCOL {
        return Err(AdminFrameError::UnexpectedPayloadProtocol(
            frame.payload_protocol,
        ));
    }
    if PayloadEncoding::try_from(frame.payload_encoding) != Ok(PayloadEncoding::None) {
        return Err(AdminFrameError::UnsupportedPayloadEncoding(
            frame.payload_encoding,
        ));
    }

    let request =
        AdminRequest::decode(frame.payload.as_slice()).map_err(AdminFrameError::Decode)?;
    let reply = render_admin_reply(snapshot, &request);
    Ok(Frame {
        envelope_version: PROTOCOL_VERSION,
        kind: FrameKind::Response as i32,
        payload_protocol: ADMIN_PAYLOAD_PROTOCOL,
        payload: reply.encode_to_vec(),
        request_id: frame.request_id,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: frame.traceparent,
        tracestate: frame.tracestate,
    })
}

/// Handle one already-accepted broker admin connection.
///
/// The connection reads one v1-framed [`Frame`] carrying an [`AdminRequest`],
/// dispatches through [`handle_admin_frame`], writes one v1-framed response
/// [`Frame`] carrying an [`AdminReply`], then returns the decoded reply for
/// tests and callers that need exit-code metadata.
pub fn handle_admin_connection<S: Read + Write>(
    stream: &mut S,
    snapshot: &AdminSnapshot,
) -> Result<AdminReply, AdminConnectionError> {
    let request_bytes = read_frame(stream)?;
    let request_frame = Frame::decode(request_bytes.as_slice())
        .map_err(AdminConnectionError::DecodeFrame)?;
    let response_frame = handle_admin_frame(request_frame, snapshot)?;
    write_frame(stream, &response_frame.encode_to_vec())?;
    AdminReply::decode(response_frame.payload.as_slice())
        .map_err(AdminConnectionError::DecodeReply)
}

/// Run one blocking local-socket accept and serve exactly one admin request.
///
/// This is the admin-side counterpart to `serve_one_local_socket` for Hello.
/// The full long-lived broker loop can reuse [`handle_admin_connection`] after
/// selecting an admin connection from the shared accept path.
pub fn serve_one_admin_socket(
    socket_path: &str,
    snapshot: &AdminSnapshot,
) -> Result<AdminReply, AdminConnectionError> {
    let listener = bind_local_socket(socket_path)?;
    let _cleanup = LocalSocketCleanup(socket_path);

    let mut stream = listener.accept()?;
    handle_admin_connection(&mut stream, snapshot)
}

/// Errors raised by admin frame validation/dispatch.
#[derive(Debug, thiserror::Error)]
pub enum AdminFrameError {
    /// Unsupported frame envelope version.
    #[error("unsupported admin frame envelope_version {0}")]
    UnsupportedEnvelopeVersion(u32),
    /// Admin frames must be requests.
    #[error("admin frame kind must be REQUEST, got {0}")]
    UnexpectedKind(i32),
    /// Admin frame used the wrong payload protocol.
    #[error("admin frame payload_protocol must be 0xAD01, got {0}")]
    UnexpectedPayloadProtocol(u32),
    /// Admin frame payload must be uncompressed.
    #[error("admin frame payload must not be compressed, got {0}")]
    UnsupportedPayloadEncoding(i32),
    /// AdminRequest protobuf decoding failed.
    #[error(transparent)]
    Decode(prost::DecodeError),
}

/// Errors raised while serving a framed admin connection.
#[derive(Debug, thiserror::Error)]
pub enum AdminConnectionError {
    /// v1 framing failed.
    #[error(transparent)]
    Framing(#[from] FramingError),
    /// The request frame could not be decoded.
    #[error("failed to decode admin request Frame: {0}")]
    DecodeFrame(prost::DecodeError),
    /// The request frame failed admin validation or dispatch.
    #[error(transparent)]
    AdminFrame(#[from] AdminFrameError),
    /// The response payload could not be decoded after dispatch.
    #[error("failed to decode admin reply payload: {0}")]
    DecodeReply(prost::DecodeError),
    /// Local socket binding failed.
    #[error(transparent)]
    LocalSocket(#[from] BrokerConnectionError),
    /// Local socket I/O failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

fn json_reply(body: String) -> AdminReply {
    AdminReply {
        kind: AdminReplyKind::Json as i32,
        body,
        exit_code: 0,
        content_type: "application/json".into(),
    }
}

fn text_reply(body: impl Into<String>, exit_code: u32) -> AdminReply {
    AdminReply {
        kind: AdminReplyKind::Text as i32,
        body: body.into(),
        exit_code,
        content_type: "text/plain".into(),
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
