//! Hello validation and in-memory negotiation.
//!
//! This module is intentionally synchronous and side-effect-free. The
//! Phase 4 accept loop will call into it after peer-credential checks,
//! rate limiting, and service-definition loading have produced the
//! registered backend table.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use prost::Message;

use crate::broker::lifecycle::names::{validate_service_name, validate_version, PipePathError};
use crate::broker::protocol::{
    hello_reply::Result as HelloReplyResult, ErrorCode, Frame, FrameKind, Hello, HelloReply,
    Negotiated, PayloadEncoding, Refused, ServiceDefinition,
};
use crate::broker::server::version_allow_list::{
    check_version_allowed, VersionPolicyBlock,
};

const PROTOCOL_VERSION: u32 = 1;
const DEFAULT_KEEPALIVE_SECS: u64 = 30 * 60;
const CONTROL_PAYLOAD_PROTOCOL: u32 = 0;

/// OS-verified peer identity for the process that sent a Hello.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerIdentity {
    /// Peer process ID from platform IPC credentials.
    pub pid: u32,
    /// User identifier or SID captured by the accept loop.
    pub uid_or_sid: String,
}

/// Decoded Hello request plus the envelope metadata that carried it.
#[derive(Clone, Debug)]
pub struct HelloRequest {
    /// Frozen v1 envelope frame. Trace context and request ID live here.
    pub frame: Frame,
    /// Decoded control-plane Hello payload.
    pub hello: Hello,
    /// OS-verified peer identity.
    pub peer: PeerIdentity,
}

impl HelloRequest {
    /// Decode a v1 control-plane Hello from a validated frame.
    pub fn decode(frame: Frame, peer: PeerIdentity) -> Result<Self, Refused> {
        if frame.envelope_version != PROTOCOL_VERSION {
            return Err(refused(
                ErrorCode::ErrorVersionUnsupported,
                "frame envelope_version is not v1",
                0,
            ));
        }
        if FrameKind::try_from(frame.kind) != Ok(FrameKind::Request) {
            return Err(refused(
                ErrorCode::ErrorPeerRejected,
                "Hello frame kind must be REQUEST",
                0,
            ));
        }
        if frame.payload_protocol != CONTROL_PAYLOAD_PROTOCOL {
            return Err(refused(
                ErrorCode::ErrorPeerRejected,
                "Hello frame payload_protocol must be control-plane",
                0,
            ));
        }
        if PayloadEncoding::try_from(frame.payload_encoding) != Ok(PayloadEncoding::None) {
            return Err(refused(
                ErrorCode::ErrorPeerRejected,
                "Hello payload must not be compressed",
                0,
            ));
        }
        let hello = Hello::decode(frame.payload.as_slice())
            .map_err(|_| refused(ErrorCode::ErrorPeerRejected, "malformed Hello payload", 0))?;
        Ok(Self { frame, hello, peer })
    }
}

/// Backend metadata already verified by the backend registry.
#[derive(Clone, Debug)]
pub struct RegisteredBackend {
    /// Service definition selected for this backend.
    pub service_definition: ServiceDefinition,
    /// Version string returned in `Negotiated.daemon_version`.
    pub daemon_version: String,
    /// Direct backend pipe/socket path returned to the client.
    pub backend_pipe: String,
    /// Capability bitmap exposed to the client.
    pub server_capabilities: u64,
}

/// Deterministic Hello handler over an in-memory backend table.
#[derive(Debug, Default)]
pub struct HelloHandler {
    backends: HashMap<String, RegisteredBackend>,
    next_connection_id: AtomicU64,
}

impl HelloHandler {
    /// Create an empty handler.
    pub fn new() -> Self {
        Self {
            backends: HashMap::new(),
            next_connection_id: AtomicU64::new(1),
        }
    }

    /// Register a backend by its service definition's service name.
    pub fn with_backend(mut self, backend: RegisteredBackend) -> Result<Self, HelloHandlerError> {
        validate_service_name_for_result(&backend.service_definition.service_name)?;
        if !backend.service_definition.min_version.is_empty() {
            validate_version_for_result(&backend.service_definition.min_version)?;
        }
        for version in &backend.service_definition.version_allow_list {
            validate_version_for_result(version)?;
        }
        self.backends
            .insert(backend.service_definition.service_name.clone(), backend);
        Ok(self)
    }

    /// Decode and handle a framed v1 Hello request.
    pub fn handle_frame(&self, frame: Frame, peer: PeerIdentity) -> HelloReply {
        match HelloRequest::decode(frame, peer) {
            Ok(request) => self.handle_request(&request),
            Err(refused) => refused_reply(refused),
        }
    }

    /// Validate a decoded Hello request and return a v1 HelloReply.
    pub fn handle_request(&self, request: &HelloRequest) -> HelloReply {
        let hello = &request.hello;
        if let Some(refused) = validate_hello_shape(hello, &request.peer) {
            return refused_reply(refused);
        }

        let Some(backend) = self.backends.get(&hello.service_name) else {
            return refused_reply(refused(
                ErrorCode::ErrorServiceUnknown,
                "service is not registered",
                0,
            ));
        };

        if let Some(refused) = validate_version_policy(hello, &backend.service_definition) {
            return refused_reply(refused);
        }

        let connection_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
        refused_or_negotiated(HelloReplyResult::Negotiated(Negotiated {
            negotiated_protocol: PROTOCOL_VERSION,
            daemon_version: backend.daemon_version.clone(),
            backend_pipe: backend.backend_pipe.clone(),
            warnings: Vec::new(),
            server_capabilities: backend.server_capabilities,
            keepalive_interval_secs: if hello.client_keepalive_secs == 0 {
                DEFAULT_KEEPALIVE_SECS
            } else {
                hello.client_keepalive_secs
            },
            handle_passed_token: Vec::new(),
            connection_id,
        }))
    }
}

/// Errors raised while constructing a handler table.
#[derive(Debug, thiserror::Error)]
pub enum HelloHandlerError {
    /// A service definition field failed validation.
    #[error(transparent)]
    PipePath(#[from] PipePathError),
}

fn validate_hello_shape(hello: &Hello, peer: &PeerIdentity) -> Option<Refused> {
    if hello.client_min_protocol > PROTOCOL_VERSION || hello.client_max_protocol < PROTOCOL_VERSION
    {
        return Some(refused(
            ErrorCode::ErrorVersionUnsupported,
            "client protocol range does not include v1",
            0,
        ));
    }
    if validate_service_name(&hello.service_name).is_err() {
        return Some(refused(
            ErrorCode::ErrorPeerRejected,
            "invalid service_name",
            0,
        ));
    }
    if hello.wanted_version.len() > 64 || validate_version(&hello.wanted_version).is_err() {
        return Some(refused(
            ErrorCode::ErrorPeerRejected,
            "invalid wanted_version",
            0,
        ));
    }
    if hello.client_version.len() > 128 {
        return Some(refused(
            ErrorCode::ErrorPeerRejected,
            "client_version exceeds 128 bytes",
            0,
        ));
    }
    if hello.client_lib_name.len() > 64 || hello.client_lib_version.len() > 64 {
        return Some(refused(
            ErrorCode::ErrorPeerRejected,
            "client_lib fields exceed 64 bytes",
            0,
        ));
    }
    if hello.peer_pid != 0 && hello.peer_pid != peer.pid {
        return Some(refused(
            ErrorCode::ErrorPeerRejected,
            "peer_pid does not match verified peer",
            0,
        ));
    }
    None
}

fn validate_version_policy(hello: &Hello, service: &ServiceDefinition) -> Option<Refused> {
    match check_version_allowed(&hello.wanted_version, service) {
        Ok(()) => None,
        Err(VersionPolicyBlock::BelowMinVersion) => Some(refused(
            ErrorCode::ErrorVersionBlocked,
            "wanted_version is below min_version",
            30_000,
        )),
        Err(VersionPolicyBlock::OutsideAllowList) => Some(refused(
            ErrorCode::ErrorVersionBlocked,
            "wanted_version is not in version_allow_list",
            30_000,
        )),
    }
}

fn validate_service_name_for_result(name: &str) -> Result<(), HelloHandlerError> {
    validate_service_name(name).map_err(HelloHandlerError::PipePath)
}

fn validate_version_for_result(version: &str) -> Result<(), HelloHandlerError> {
    validate_version(version).map_err(HelloHandlerError::PipePath)
}

fn refused(code: ErrorCode, reason: impl Into<String>, retry_after_ms: u64) -> Refused {
    Refused {
        reason: reason.into(),
        daemon_min_protocol: PROTOCOL_VERSION,
        daemon_max_protocol: PROTOCOL_VERSION,
        code: code as i32,
        details: HashMap::new(),
        retry_after_ms,
    }
}

fn refused_reply(refused: Refused) -> HelloReply {
    refused_or_negotiated(HelloReplyResult::Refused(refused))
}

fn refused_or_negotiated(result: HelloReplyResult) -> HelloReply {
    HelloReply {
        result: Some(result),
    }
}
