//! Service-definition-backed Hello routing.
//!
//! `HelloHandler` owns deterministic validation and in-memory negotiation.
//! `HelloRouter` adds the broker-facing lookup layer: reload the service
//! definition for each request, resolve the trust-domain instance, query the
//! backend registry, and then delegate the final reply construction to
//! `HelloHandler`.

use std::collections::HashMap;

use crate::broker::protocol::{
    hello_reply::Result as HelloReplyResult, ErrorCode, Frame, HelloReply, Refused,
};
use crate::broker::server::{
    check_version_allowed, BackendRegistry, BrokerInstanceKey, HelloHandler, HelloHandlerError,
    HelloRequest, PeerIdentity, RegisteredBackend, ServiceDefinitionError, ServiceDefinitionLoader,
    VersionPolicyBlock,
};

const PROTOCOL_VERSION: u32 = 1;

/// Routes decoded Hello requests through service definitions and backend state.
#[derive(Clone, Copy)]
pub struct HelloRouter<'a> {
    service_definitions: &'a ServiceDefinitionLoader,
    backends: &'a BackendRegistry,
}

impl<'a> HelloRouter<'a> {
    /// Create a router over immutable broker state.
    pub fn new(
        service_definitions: &'a ServiceDefinitionLoader,
        backends: &'a BackendRegistry,
    ) -> Self {
        Self {
            service_definitions,
            backends,
        }
    }

    /// Decode and route a framed Hello request.
    pub fn handle_frame(&self, frame: Frame, peer: PeerIdentity) -> HelloReply {
        match HelloRequest::decode(frame, peer) {
            Ok(request) => self.handle_request(&request),
            Err(refused) => refused_reply(refused),
        }
    }

    /// Route a decoded Hello request.
    pub fn handle_request(&self, request: &HelloRequest) -> HelloReply {
        match self.route_request(request) {
            Ok(registered) => match HelloHandler::new().with_backend(registered) {
                Ok(handler) => handler.handle_request(request),
                Err(err) => refused_reply(refused_from_handler_error(err)),
            },
            Err(refused) => refused_reply(refused),
        }
    }

    fn route_request(&self, request: &HelloRequest) -> Result<RegisteredBackend, Refused> {
        let service_definition = self
            .service_definitions
            .lookup_or_reload(&request.hello.service_name)
            .map_err(refused_from_service_definition_error)?;

        if let Err(block) = check_version_allowed(&request.hello.wanted_version, &service_definition)
        {
            return Err(refused_from_version_policy(block));
        }

        let instance = BrokerInstanceKey::from_service_definition(&service_definition).map_err(
            |err| {
                refused(
                    ErrorCode::ErrorInternal,
                    format!("service isolation could not be resolved: {err}"),
                    0,
                )
            },
        )?;

        self.backends
            .registered_backend_for(&instance, &service_definition, &request.hello.wanted_version)
            .ok_or_else(|| {
                refused(
                    ErrorCode::ErrorBackendSpawnFailed,
                    "backend is not registered",
                    1_000,
                )
            })
    }
}

fn refused_from_service_definition_error(error: ServiceDefinitionError) -> Refused {
    match error {
        ServiceDefinitionError::InvalidName(_) => refused(
            ErrorCode::ErrorPeerRejected,
            "invalid service_name",
            0,
        ),
        ServiceDefinitionError::Io(err) if err.kind() == std::io::ErrorKind::NotFound => refused(
            ErrorCode::ErrorServiceUnknown,
            "service definition was not found",
            0,
        ),
        other => refused(
            ErrorCode::ErrorServiceUnknown,
            format!("service definition could not be loaded: {other}"),
            0,
        ),
    }
}

fn refused_from_version_policy(block: VersionPolicyBlock) -> Refused {
    match block {
        VersionPolicyBlock::BelowMinVersion => refused(
            ErrorCode::ErrorVersionBlocked,
            "wanted_version is below min_version",
            30_000,
        ),
        VersionPolicyBlock::OutsideAllowList => refused(
            ErrorCode::ErrorVersionBlocked,
            "wanted_version is not in version_allow_list",
            30_000,
        ),
    }
}

fn refused_from_handler_error(error: HelloHandlerError) -> Refused {
    refused(
        ErrorCode::ErrorInternal,
        format!("registered backend could not be installed: {error}"),
        0,
    )
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
    HelloReply {
        result: Some(HelloReplyResult::Refused(refused)),
    }
}
