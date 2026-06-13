//! Service supervision (runpm) request handlers — Phase 2 (#222).
//!
//! `start`, `stop`, `restart`, `delete`, `list`, and `describe` (show) are
//! implemented end-to-end against the SQLite-backed
//! [`crate::daemon::services::ServiceRegistry`]. `logs`/`flush` (Phase 3) and
//! `save`/`resurrect` (Phase 4) remain stubs that acknowledge the RPC.

use std::collections::HashMap;

use crate::daemon::services::{ServiceDef, ServiceError, ServiceRecord};
use crate::proto::daemon::{
    DaemonRequest, DaemonResponse, ServiceConfig, ServiceDeleteResponse, ServiceDescribeResponse,
    ServiceFlushResponse, ServiceListResponse, ServiceLogsResponse, ServiceRestartResponse,
    ServiceResurrectResponse, ServiceSaveResponse, ServiceStartResponse, ServiceState,
    ServiceStopResponse, StatusCode,
};

use super::DaemonState;

// ---------------------------------------------------------------------------
// Proto <-> domain mapping
// ---------------------------------------------------------------------------

fn def_from_config(config: ServiceConfig) -> ServiceDef {
    ServiceDef {
        name: config.name,
        cmd: config.cmd,
        cwd: config.cwd,
        env: config.env.into_iter().collect(),
        autorestart: config.autorestart,
        max_restarts: config.max_restarts,
        restart_delay_ms: config.restart_delay_ms,
        kill_timeout_ms: config.kill_timeout_ms,
        min_uptime_ms: config.min_uptime_ms,
    }
}

fn config_from_def(def: &ServiceDef) -> ServiceConfig {
    let env: HashMap<String, String> = def.env.iter().cloned().collect();
    ServiceConfig {
        name: def.name.clone(),
        cmd: def.cmd.clone(),
        cwd: def.cwd.clone(),
        env,
        autorestart: def.autorestart,
        max_restarts: def.max_restarts,
        restart_delay_ms: def.restart_delay_ms,
        kill_timeout_ms: def.kill_timeout_ms,
        min_uptime_ms: def.min_uptime_ms,
    }
}

fn state_from_record(rec: &ServiceRecord) -> ServiceState {
    ServiceState {
        name: rec.def.name.clone(),
        id: rec.id,
        status: rec.status.as_str().to_string(),
        pid: rec.pid,
        restart_count: rec.restart_count,
        last_started_at: rec.last_started_at,
        last_exited_at: rec.last_exited_at,
        last_exit_code: rec.last_exit_code,
        config: Some(config_from_def(&rec.def)),
    }
}

/// Map a [`ServiceError`] to the closest protobuf [`StatusCode`].
fn status_for(err: &ServiceError) -> StatusCode {
    match err {
        ServiceError::NotFound(_) => StatusCode::NotFound,
        ServiceError::AlreadyExists(_) | ServiceError::InvalidConfig(_) => {
            StatusCode::InvalidArgument
        }
        ServiceError::Spawn(_) | ServiceError::Db(_) => StatusCode::Internal,
    }
}

fn err_response(request: &DaemonRequest, err: ServiceError) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: status_for(&err) as i32,
        message: err.to_string(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Handlers — implemented (Phase 2)
// ---------------------------------------------------------------------------

/// `ServiceStart`: create/update a service definition and launch it.
pub fn handle_service_start(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let Some(req) = request.service_start.as_ref() else {
        return err_response(
            request,
            ServiceError::InvalidConfig("missing service_start payload".into()),
        );
    };
    let Some(config) = req.config.clone() else {
        return err_response(
            request,
            ServiceError::InvalidConfig("missing service config".into()),
        );
    };
    match state.services.start(def_from_config(config)) {
        Ok(rec) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            service_start: Some(ServiceStartResponse {
                service: Some(state_from_record(&rec)),
            }),
            ..Default::default()
        },
        Err(e) => err_response(request, e),
    }
}

/// `ServiceStop`: stop the targeted service(s).
pub fn handle_service_stop(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let target = request
        .service_stop
        .as_ref()
        .map(|r| r.target.clone())
        .unwrap_or_default();
    match state.services.stop(&target) {
        Ok(stopped_count) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            service_stop: Some(ServiceStopResponse { stopped_count }),
            ..Default::default()
        },
        Err(e) => err_response(request, e),
    }
}

/// `ServiceRestart`: stop + start the targeted service(s), bumping counts.
pub fn handle_service_restart(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let target = request
        .service_restart
        .as_ref()
        .map(|r| r.target.clone())
        .unwrap_or_default();
    match state.services.restart(&target) {
        Ok(restarted_count) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            service_restart: Some(ServiceRestartResponse { restarted_count }),
            ..Default::default()
        },
        Err(e) => err_response(request, e),
    }
}

/// `ServiceDelete`: stop (if running) and remove the targeted service(s).
pub fn handle_service_delete(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let target = request
        .service_delete
        .as_ref()
        .map(|r| r.target.clone())
        .unwrap_or_default();
    match state.services.delete(&target) {
        Ok(deleted_count) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            service_delete: Some(ServiceDeleteResponse { deleted_count }),
            ..Default::default()
        },
        Err(e) => err_response(request, e),
    }
}

/// `ServiceList`: return all service definitions + state.
pub fn handle_service_list(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    match state.services.list() {
        Ok(records) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            service_list: Some(ServiceListResponse {
                services: records.iter().map(state_from_record).collect(),
            }),
            ..Default::default()
        },
        Err(e) => err_response(request, e),
    }
}

/// `ServiceDescribe` (show): return one service by name or id.
pub fn handle_service_describe(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let target = request
        .service_describe
        .as_ref()
        .map(|r| r.target.clone())
        .unwrap_or_default();
    match state.services.describe(&target) {
        Ok(rec) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            service_describe: Some(ServiceDescribeResponse {
                service: Some(state_from_record(&rec)),
            }),
            ..Default::default()
        },
        Err(e) => err_response(request, e),
    }
}

// ---------------------------------------------------------------------------
// Handlers — stubs (Phase 3 / Phase 4)
// ---------------------------------------------------------------------------

/// Phase 3 stub for `ServiceLogs` — log tailing/follow ships in Phase 3 (#222).
pub fn handle_service_logs(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_logs: Some(ServiceLogsResponse::default()),
        ..Default::default()
    }
}

/// Phase 3 stub for `ServiceFlush` — log flush ships in Phase 3 (#222).
pub fn handle_service_flush(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_flush: Some(ServiceFlushResponse::default()),
        ..Default::default()
    }
}

/// Phase 4 stub for `ServiceSave` — snapshot persistence ships in Phase 4 (#222).
pub fn handle_service_save(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_save: Some(ServiceSaveResponse::default()),
        ..Default::default()
    }
}

/// Phase 4 stub for `ServiceResurrect` — snapshot restore ships in Phase 4 (#222).
pub fn handle_service_resurrect(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_resurrect: Some(ServiceResurrectResponse::default()),
        ..Default::default()
    }
}
