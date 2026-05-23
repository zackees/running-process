//! Service supervision (runpm) — Phase 1 stubs.
//!
//! The handlers below acknowledge the new SERVICE_* request types so the
//! wire protocol round-trips successfully while the real supervisor lands
//! in Phase 2 of #106. Each returns StatusCode::Ok with a default-valued
//! response payload — no service state is created, mutated, or persisted.

use crate::proto::daemon::{
    DaemonRequest, DaemonResponse, ServiceDeleteResponse, ServiceDescribeResponse,
    ServiceFlushResponse, ServiceListResponse, ServiceLogsResponse, ServiceRestartResponse,
    ServiceResurrectResponse, ServiceSaveResponse, ServiceStartResponse, ServiceStopResponse,
    StatusCode,
};

use super::DaemonState;

/// Phase 1 stub for `ServiceStart` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_start(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_start: Some(ServiceStartResponse::default()),
        ..Default::default()
    }
}

/// Phase 1 stub for `ServiceStop` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_stop(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_stop: Some(ServiceStopResponse::default()),
        ..Default::default()
    }
}

/// Phase 1 stub for `ServiceRestart` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_restart(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_restart: Some(ServiceRestartResponse::default()),
        ..Default::default()
    }
}

/// Phase 1 stub for `ServiceDelete` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_delete(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_delete: Some(ServiceDeleteResponse::default()),
        ..Default::default()
    }
}

/// Phase 1 stub for `ServiceList` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_list(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_list: Some(ServiceListResponse::default()),
        ..Default::default()
    }
}

/// Phase 1 stub for `ServiceDescribe` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_describe(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_describe: Some(ServiceDescribeResponse::default()),
        ..Default::default()
    }
}

/// Phase 1 stub for `ServiceLogs` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_logs(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_logs: Some(ServiceLogsResponse::default()),
        ..Default::default()
    }
}

/// Phase 1 stub for `ServiceFlush` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_flush(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_flush: Some(ServiceFlushResponse::default()),
        ..Default::default()
    }
}

/// Phase 1 stub for `ServiceSave` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_save(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_save: Some(ServiceSaveResponse::default()),
        ..Default::default()
    }
}

/// Phase 1 stub for `ServiceResurrect` — real lifecycle ships in Phase 2 (#106).
pub fn handle_service_resurrect(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        service_resurrect: Some(ServiceResurrectResponse::default()),
        ..Default::default()
    }
}
