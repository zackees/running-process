//! HTTP verbs, each a thin translation onto [`ProbeOps`] (S13 / #642).
//!
//! Every handler does the same three things: parse a query string into the
//! same domain type the socket ingress builds, call the shared core, and
//! serialize the result. No handler contains policy. That is what makes
//! transport parity a structural property rather than a promise — an env
//! value the socket withholds is withheld here because it is the *same
//! function* deciding, not a second implementation that agrees for now.

use std::collections::BTreeMap;
use std::num::NonZeroU32;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::crash_query::{CrashFilter, MAX_CRASH_LIMIT};
use crate::http::HttpState;
use crate::probe_ops::{ProbeReply, ProbeRequest};
use crate::query::ProcessQuery;

/// Default page size when a caller omits `limit`.
///
/// The engine refuses an absent limit, and rightly — but a browser hitting
/// `/v1/ps` has no way to have known that, and a 400 would make the landing
/// page look broken. HTTP supplies a visible default instead; the *engine*
/// still has no default, which is where it matters.
pub const DEFAULT_LIMIT: u32 = 100;

/// One error shape for every route, so a client has one thing to parse.
#[derive(Debug, Serialize)]
pub struct ApiError {
    /// Stable machine-readable classification.
    pub error: String,
    /// Human-readable detail.
    pub detail: String,
}

impl ApiError {
    fn new(error: &str, detail: impl Into<String>) -> (StatusCode, Json<Self>) {
        (
            StatusCode::BAD_REQUEST,
            Json(Self {
                error: error.to_string(),
                detail: detail.into(),
            }),
        )
    }
}

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ApiError>)>;

/// `GET /v1/ps` — live process query.
#[derive(Debug, Default, Deserialize)]
pub struct PsParams {
    /// Glob on process name.
    pub name: Option<String>,
    /// Regex on process name.
    pub name_regex: Option<String>,
    /// Glob on working directory.
    pub cwd: Option<String>,
    /// Exact application class.
    pub app_class: Option<String>,
    /// Include processes that never registered.
    #[serde(default)]
    pub include_unregistered: bool,
    /// Return allowlisted env values.
    #[serde(default)]
    pub include_env: bool,
    /// Maximum results.
    pub limit: Option<u32>,
}

/// One process row, as JSON.
#[derive(Debug, Serialize)]
pub struct ProcessRow {
    /// OS process id.
    pub pid: u64,
    /// Process name.
    pub name: String,
    /// Executable path, empty when undisclosed.
    pub exe: String,
    /// Working directory, empty when undisclosed.
    pub cwd: String,
    /// Whether an ARMED registration backs this row.
    pub registered: bool,
    /// Declared application class.
    pub app_class: String,
    /// Declared application name.
    pub app_name: String,
    /// Allowlisted environment values. Never anything else.
    pub env: BTreeMap<String, String>,
    /// Disclosed environment variable names.
    pub env_names: Vec<String>,
}

/// Answer a live process query, identically to the socket's `ProcessQuery`.
pub async fn ps(
    State(state): State<HttpState>,
    Query(params): Query<PsParams>,
) -> ApiResult<Vec<ProcessRow>> {
    let mut wire = running_process_probe::probe_diag::v1::ProcessQuery {
        limit: params.limit.unwrap_or(DEFAULT_LIMIT),
        include_unregistered: params.include_unregistered,
        include_env: params.include_env,
        ..Default::default()
    };
    if let Some(name) = params.name {
        wire.name_glob = name;
    }
    if let Some(name) = params.name_regex {
        wire.name_regex = name;
    }
    if let Some(cwd) = params.cwd {
        wire.cwd_glob = cwd;
    }
    if let Some(class) = params.app_class {
        wire.app_class = class;
    }

    let query = ProcessQuery::from_proto(wire)
        .map_err(|error| ApiError::new("invalid_query", error.to_string()))?;

    match dispatch(&state, ProbeRequest::Query(Box::new(query))) {
        ProbeReply::Processes(processes) => Ok(Json(
            processes
                .into_iter()
                .map(|info| ProcessRow {
                    pid: info.key.as_ref().map(|k| k.pid).unwrap_or_default(),
                    name: info.name,
                    exe: info.exe_path,
                    cwd: info.cwd,
                    registered: info.registered,
                    app_class: info.app_class,
                    app_name: info.app_name,
                    env: info.env.into_iter().collect(),
                    env_names: info.env_names,
                })
                .collect(),
        )),
        other => Err(refusal(other)),
    }
}

/// `GET /v1/crashes` and `/v1/crashes/stats` share this filter.
#[derive(Debug, Default, Deserialize)]
pub struct CrashParams {
    /// Exact application class.
    pub class: Option<String>,
    /// `LIKE` pattern on application class.
    pub class_like: Option<String>,
    /// Exact crash signature.
    pub signature: Option<String>,
    /// Inclusive lower bound, unix milliseconds.
    pub since: Option<u64>,
    /// Exclusive upper bound, unix milliseconds.
    pub until: Option<u64>,
    /// Maximum records. Ignored by the stats route.
    pub limit: Option<u32>,
}

impl CrashParams {
    fn filter(&self) -> CrashFilter {
        CrashFilter {
            app_class: self.class.clone(),
            app_class_like: self.class_like.clone(),
            app_name: None,
            instance_name: None,
            signature: self.signature.clone(),
            since_unix_ms: self.since,
            until_unix_ms: self.until,
        }
    }
}

/// One crash row, as JSON. Redacted metadata only — see [`crate::crash_query`].
#[derive(Debug, Serialize)]
pub struct CrashRow {
    /// Opaque artifact handle, resolved by `/v1/artifacts/{id}`.
    pub id: i64,
    /// Application class.
    pub app_class: String,
    /// Application name.
    pub app_name: String,
    /// Instance discriminator.
    pub instance_name: String,
    /// Crashed process id.
    pub pid: u32,
    /// Crash signature.
    pub signature: String,
    /// Signal name or exception code.
    pub fault_kind: String,
    /// Crash time, unix milliseconds.
    pub crashed_at_ms: u64,
    /// Artifact size, so a caller can decide whether to fetch it.
    pub artifact_bytes: u64,
}

/// Page through durable crash history.
///
/// The requested limit is clamped to the store's maximum rather than refused:
/// a browser asking for more than the daemon will page is not an error, it is
/// a browser being told what the maximum is by receiving it.
pub async fn crashes(
    State(state): State<HttpState>,
    Query(params): Query<CrashParams>,
) -> ApiResult<Vec<CrashRow>> {
    let requested = params.limit.unwrap_or(DEFAULT_LIMIT);
    let limit = NonZeroU32::new(requested.min(MAX_CRASH_LIMIT))
        .ok_or_else(|| ApiError::new("invalid_query", "limit must be greater than zero"))?;

    let request = ProbeRequest::QueryCrashes {
        filter: Box::new(params.filter()),
        limit,
    };
    match dispatch(&state, request) {
        ProbeReply::Crashes(records) => Ok(Json(
            records
                .into_iter()
                .map(|record| CrashRow {
                    id: record.id,
                    app_class: record.app_class,
                    app_name: record.app_name,
                    instance_name: record.instance_name,
                    pid: record.pid,
                    signature: record.signature,
                    fault_kind: record.fault_kind,
                    crashed_at_ms: record.crashed_at_ms,
                    artifact_bytes: record.artifact_bytes,
                    // `artifact_path` is deliberately not projected. It is
                    // daemon-private and discloses the owner's directory
                    // layout; the opaque `id` is what a caller needs.
                })
                .collect(),
        )),
        other => Err(refusal(other)),
    }
}

/// One signature rollup, as JSON.
#[derive(Debug, Serialize)]
pub struct SignatureRow {
    /// The crash signature.
    pub signature: String,
    /// Matching crashes carrying it.
    pub count: u64,
    /// Earliest occurrence, unix milliseconds.
    pub first_unix_ms: u64,
    /// Latest occurrence, unix milliseconds.
    pub last_unix_ms: u64,
    /// Distinct classes it appeared in.
    pub app_classes: Vec<String>,
}

/// The rollup response.
#[derive(Debug, Serialize)]
pub struct StatsResponse {
    /// Per-signature rollups, most frequent first.
    pub signatures: Vec<SignatureRow>,
    /// Total matching crashes over the whole match set, not a page of it.
    pub total: u64,
    /// Earliest match, unix milliseconds.
    pub first_unix_ms: u64,
    /// Latest match, unix milliseconds.
    pub last_unix_ms: u64,
    /// Distinct classes among matches.
    pub distinct_classes: u64,
}

/// Roll crash history up by signature.
///
/// Takes no limit, because the result is bounded by the number of distinct
/// signatures rather than the number of crashes.
pub async fn crash_stats(
    State(state): State<HttpState>,
    Query(params): Query<CrashParams>,
) -> ApiResult<StatsResponse> {
    match dispatch(&state, ProbeRequest::CrashStats(Box::new(params.filter()))) {
        ProbeReply::CrashStatistics(stats) => Ok(Json(StatsResponse {
            signatures: stats
                .signatures
                .iter()
                .map(|stat| SignatureRow {
                    signature: stat.signature.clone(),
                    count: stat.count,
                    first_unix_ms: stat.first_unix_ms,
                    last_unix_ms: stat.last_unix_ms,
                    app_classes: stat.app_classes.clone(),
                })
                .collect(),
            total: stats.total,
            first_unix_ms: stats.first_unix_ms,
            last_unix_ms: stats.last_unix_ms,
            distinct_classes: stats.distinct_classes,
        })),
        other => Err(refusal(other)),
    }
}

/// `POST /v1/snapshot` — ask a registered process for an all-thread capture.
#[derive(Debug, Deserialize)]
pub struct SnapshotParams {
    /// Target process id.
    pub pid: u32,
    /// Process start time, which is what makes the pid an identity.
    pub start_time: u64,
    /// Host boot id.
    #[serde(default)]
    pub boot_id: String,
    /// Maximum native frames per thread.
    #[serde(default)]
    pub max_depth: u32,
}

/// The accepted-capture response.
#[derive(Debug, Serialize)]
pub struct SnapshotResponse {
    /// Job id to poll, empty when the capture completed inline.
    pub job_id: String,
    /// Current job state.
    pub state: i32,
}

/// Ask a registered process for an all-thread stack capture.
pub async fn snapshot(
    State(state): State<HttpState>,
    Query(params): Query<SnapshotParams>,
) -> ApiResult<SnapshotResponse> {
    let request = ProbeRequest::CaptureStack {
        key: crate::registry::ProcessKey {
            pid: params.pid,
            started_at_unix_ms: params.start_time,
            boot_id: params.boot_id,
        },
        max_depth: params.max_depth,
        thread_filter: 0,
        deadline_unix_ms: 0,
    };
    match dispatch(&state, request) {
        ProbeReply::CaptureAccepted(reply) => Ok(Json(SnapshotResponse {
            job_id: reply.job_id,
            state: 0,
        })),
        ProbeReply::JobStatus(status) => Ok(Json(SnapshotResponse {
            job_id: status.job_id,
            state: status.state,
        })),
        other => Err(refusal(other)),
    }
}

/// `GET /v1/flame` — a collapsed-stack artifact as a tree the UI can draw.
#[derive(Debug, Deserialize)]
pub struct FlameParams {
    /// Artifact id to render.
    pub artifact: i64,
}

/// One node of the flame tree.
#[derive(Debug, Default, Serialize)]
pub struct FlameNode {
    /// Frame name.
    pub name: String,
    /// Samples in this subtree.
    pub value: u64,
    /// Callees.
    pub children: Vec<FlameNode>,
}

/// Render one collapsed-stack artifact as a tree the UI can draw.
pub async fn flame(
    State(state): State<HttpState>,
    Query(params): Query<FlameParams>,
) -> ApiResult<FlameNode> {
    let store = state
        .ops()
        .crash_store()
        .ok_or_else(|| ApiError::new("unavailable", "this daemon has no artifact store"))?;
    let guard = store
        .begin_fetch(params.artifact)
        .map_err(|error| ApiError::new("unavailable", error.to_string()))?
        .ok_or_else(|| ApiError::new("not_found", "no artifact with that id"))?;

    let text = std::fs::read_to_string(guard.path())
        .map_err(|error| ApiError::new("unreadable", error.to_string()))?;
    Ok(Json(collapsed_to_tree(&text)))
}

/// Fold collapsed-stack lines (`a;b;c 42`) into a tree.
///
/// The collapsed format is one line per unique stack with a sample count, so
/// folding is a prefix merge. Malformed lines are skipped rather than failing
/// the request: a profile is a sampled artifact, and losing one line of it is
/// strictly better than showing the operator nothing.
pub fn collapsed_to_tree(text: &str) -> FlameNode {
    let mut root = FlameNode {
        name: "root".to_string(),
        ..FlameNode::default()
    };

    for line in text.lines() {
        let line = line.trim();
        let Some((stack, count)) = line.rsplit_once(' ') else {
            continue;
        };
        let Ok(count) = count.parse::<u64>() else {
            continue;
        };
        root.value += count;

        let mut node = &mut root;
        for frame in stack.split(';').filter(|f| !f.is_empty()) {
            let index = match node.children.iter().position(|c| c.name == frame) {
                Some(index) => index,
                None => {
                    node.children.push(FlameNode {
                        name: frame.to_string(),
                        ..FlameNode::default()
                    });
                    node.children.len() - 1
                }
            };
            node = &mut node.children[index];
            node.value += count;
        }
    }
    root
}

/// Run a request through the shared core as the daemon's own owner.
///
/// The peer identity is the daemon's owner because the bearer token already
/// answered "is this the owner" — a token this caller could only have read
/// from an owner-only discovery file. `ProbeOps` still applies every policy
/// below that: env allowlists, ARMED state, disclosure flags.
fn dispatch(state: &HttpState, request: ProbeRequest) -> ProbeReply {
    let peer = running_process::broker::server::PeerIdentity {
        pid: std::process::id(),
        uid_or_sid: state.ops().owner(),
    };
    state.ops().dispatch(
        request,
        &peer,
        crate::serve::next_conn_id(),
        crate::probe_ops::IdentityVerdict {
            verified: true,
            connection_alive: true,
        },
    )
}

/// Turn a refusal into an HTTP error, preserving the daemon's reason.
fn refusal(reply: ProbeReply) -> (StatusCode, Json<ApiError>) {
    match reply {
        ProbeReply::Refused { code, reason } => (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: format!("{code:?}"),
                detail: reason,
            }),
        ),
        ProbeReply::CrashRefused { code, reason, .. } => (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: format!("{code:?}"),
                detail: reason,
            }),
        ),
        other => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: "unexpected_reply".to_string(),
                detail: format!("{other:?}"),
            }),
        ),
    }
}
