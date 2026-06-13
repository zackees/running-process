//! One-call broker adoption: negotiate → dial → ready-to-talk client (#433 R1).
//!
//! [`connect_to_backend`] returns a raw
//! [`BackendConnection`] — a bare
//! socket the consumer must still wrap in a [`FrameClient`] before it can send
//! a single request. Every consumer (zccache, soldr, clud, fbuild) repeats the
//! same three lines: check the disable env, call `connect_to_backend`, wrap the
//! stream. [`BrokerSession::adopt`] is that recipe, owned once here so the
//! contract is a single call:
//!
//! ```no_run
//! use running_process::broker::adopt::BrokerSession;
//! use running_process::broker::client::ConnectBackendRequest;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let request = ConnectBackendRequest::new("broker.sock", "zccache", "1.11.20", "1.11.20");
//! let mut session = BrokerSession::adopt(request)?;
//! let reply = session.request(0x7A63, b"ping".to_vec())?;
//! assert_eq!(reply.payload, b"pong");
//! # Ok(()) }
//! ```
//!
//! The blocking [`BrokerSession`] is the wire-of-record; the async
//! [`AsyncBrokerSession`] (feature `client-async`, #433 R3) is a thin
//! `spawn_blocking` wrapper so tokio daemons get the same one-call adoption
//! without re-implementing the negotiation against `AsyncRead`/`AsyncWrite`.

use crate::broker::backend_sdk::{FrameClient, FrameClientError};
use crate::broker::client::{
    broker_disabled_by_env, connect_to_backend, BackendConnection, BackendConnectionRoute,
    BrokerClientError, BrokerDisableEnvError, ConnectBackendRequest,
};
use crate::broker::protocol::{Frame, Negotiated};

/// A negotiated, dialed, and framed broker backend connection.
///
/// Produced by [`BrokerSession::adopt`]. Wraps the
/// [`BackendConnection`] stream in a
/// [`FrameClient`] so the caller can issue correlated request/response frames
/// immediately, while still exposing how the connection was reached
/// ([`route`](Self::route)), the cacheable [`endpoint`](Self::endpoint), and the
/// broker's [`negotiated`](Self::negotiated) metadata.
pub struct BrokerSession {
    client: FrameClient,
    route: BackendConnectionRoute,
    endpoint: String,
    negotiated: Option<Negotiated>,
}

impl BrokerSession {
    /// Negotiate through the broker and return a ready-to-talk session.
    ///
    /// Honours the canonical escape hatch first: if
    /// `RUNNING_PROCESS_DISABLE=1` is set, this returns
    /// [`AdoptError::BrokerDisabled`] so the consumer falls back to its direct
    /// path instead of silently dialing the broker. An invalid disable value
    /// surfaces as [`AdoptError::DisableEnv`].
    pub fn adopt(request: ConnectBackendRequest<'_>) -> Result<Self, AdoptError> {
        if broker_disabled_by_env()? {
            return Err(AdoptError::BrokerDisabled);
        }
        Ok(Self::from_connection(connect_to_backend(request)?))
    }

    fn from_connection(connection: BackendConnection) -> Self {
        Self {
            client: FrameClient::from_stream(connection.stream),
            route: connection.route,
            endpoint: connection.endpoint,
            negotiated: connection.negotiated,
        }
    }

    /// How the backend connection was reached.
    pub fn route(&self) -> BackendConnectionRoute {
        self.route
    }

    /// Negotiated backend endpoint, suitable as a Hello-skip cache key.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Broker negotiation metadata, present when the broker path was used.
    pub fn negotiated(&self) -> Option<&Negotiated> {
        self.negotiated.as_ref()
    }

    /// Send one correlated request and await its response frame.
    pub fn request(
        &mut self,
        payload_protocol: u32,
        payload: Vec<u8>,
    ) -> Result<Frame, FrameClientError> {
        self.client.request(payload_protocol, payload)
    }

    /// Borrow the underlying frame client for advanced use.
    pub fn client_mut(&mut self) -> &mut FrameClient {
        &mut self.client
    }

    /// Consume the session and return the owned frame client.
    pub fn into_client(self) -> FrameClient {
        self.client
    }
}

/// Errors from [`BrokerSession::adopt`] / [`AsyncBrokerSession::adopt`].
#[derive(Debug, thiserror::Error)]
pub enum AdoptError {
    /// `RUNNING_PROCESS_DISABLE=1` is set — the caller should use its direct
    /// (non-broker) path. Not a failure of the broker itself.
    #[error("broker disabled via RUNNING_PROCESS_DISABLE=1; use the direct path")]
    BrokerDisabled,
    /// The disable env var held an invalid value.
    #[error(transparent)]
    DisableEnv(#[from] BrokerDisableEnvError),
    /// Broker negotiation or backend dial failed. Use
    /// [`BrokerClientError::refusal_kind`] to branch on broker refusals.
    #[error(transparent)]
    Connect(#[from] BrokerClientError),
    /// The async adoption worker thread failed to join (panicked or was
    /// cancelled). Only reachable on the `client-async` path.
    #[cfg(feature = "client-async")]
    #[error("async adopt worker failed to join: {0}")]
    AsyncJoin(String),
}

/// Owned inputs for [`AsyncBrokerSession::adopt`] (#433 R3).
///
/// The blocking [`ConnectBackendRequest`] borrows `&str`, which cannot cross a
/// `spawn_blocking` boundary. This owned mirror carries the same fields by
/// value; [`AsyncBrokerSession::adopt`] reconstructs a borrowed
/// [`ConnectBackendRequest`] from it inside the worker thread.
#[cfg(feature = "client-async")]
#[derive(Clone, Debug)]
pub struct OwnedConnectRequest {
    /// Broker pipe/socket endpoint.
    pub broker_endpoint: String,
    /// Logical service name, such as `zccache`.
    pub service_name: String,
    /// Backend version the caller wants.
    pub wanted_version: String,
    /// Version of the caller's own service binary.
    pub self_version: String,
    /// Previously negotiated backend endpoint, if the caller has one.
    pub cached_backend_endpoint: Option<String>,
    /// Informational client version.
    pub client_version: String,
    /// Client library name for diagnostics.
    pub client_lib_name: String,
    /// Client library version for diagnostics.
    pub client_lib_version: String,
    /// Proposed keepalive interval.
    pub client_keepalive_secs: u64,
    /// Opt in to adopting a handed-off backend connection.
    pub adopt_handed_off_connection: bool,
    /// Deadline for the handoff-ready relay when adoption is enabled.
    pub handoff_ready_timeout: std::time::Duration,
}

#[cfg(feature = "client-async")]
impl OwnedConnectRequest {
    /// Build an owned request with running-process defaults.
    pub fn new(
        broker_endpoint: impl Into<String>,
        service_name: impl Into<String>,
        wanted_version: impl Into<String>,
        self_version: impl Into<String>,
    ) -> Self {
        Self {
            broker_endpoint: broker_endpoint.into(),
            service_name: service_name.into(),
            wanted_version: wanted_version.into(),
            self_version: self_version.into(),
            cached_backend_endpoint: None,
            client_version: String::new(),
            client_lib_name: "running-process".to_string(),
            client_lib_version: env!("CARGO_PKG_VERSION").to_string(),
            client_keepalive_secs: 0,
            adopt_handed_off_connection: false,
            handoff_ready_timeout: crate::broker::client::DEFAULT_HANDOFF_READY_TIMEOUT,
        }
    }

    fn as_request(&self) -> ConnectBackendRequest<'_> {
        ConnectBackendRequest {
            broker_endpoint: &self.broker_endpoint,
            service_name: &self.service_name,
            wanted_version: &self.wanted_version,
            self_version: &self.self_version,
            cached_backend_endpoint: self.cached_backend_endpoint.as_deref(),
            client_version: &self.client_version,
            client_lib_name: &self.client_lib_name,
            client_lib_version: &self.client_lib_version,
            client_keepalive_secs: self.client_keepalive_secs,
            adopt_handed_off_connection: self.adopt_handed_off_connection,
            handoff_ready_timeout: self.handoff_ready_timeout,
        }
    }
}

/// Async counterpart of [`BrokerSession`] for tokio daemons (#433 R3).
///
/// Runs the blocking negotiation on `tokio::task::spawn_blocking` and wraps the
/// resulting [`FrameClient`] in an [`AsyncFrameClient`] so every later request
/// is `.await`-able without a manual `spawn_blocking` at the call site.
///
/// [`AsyncFrameClient`]: crate::broker::backend_sdk::AsyncFrameClient
#[cfg(feature = "client-async")]
pub struct AsyncBrokerSession {
    client: crate::broker::backend_sdk::AsyncFrameClient,
    route: BackendConnectionRoute,
    endpoint: String,
    negotiated: Option<Negotiated>,
}

#[cfg(feature = "client-async")]
impl AsyncBrokerSession {
    /// Negotiate through the broker on a blocking worker and return a
    /// ready-to-talk async session.
    pub async fn adopt(request: OwnedConnectRequest) -> Result<Self, AdoptError> {
        let joined = tokio::task::spawn_blocking(move || {
            BrokerSession::adopt(request.as_request()).map(|session| {
                (
                    session.route,
                    session.endpoint,
                    session.negotiated,
                    session.client,
                )
            })
        })
        .await
        .map_err(|err| AdoptError::AsyncJoin(err.to_string()))?;
        let (route, endpoint, negotiated, client) = joined?;
        Ok(Self {
            client: crate::broker::backend_sdk::AsyncFrameClient::from_blocking(client),
            route,
            endpoint,
            negotiated,
        })
    }

    /// How the backend connection was reached.
    pub fn route(&self) -> BackendConnectionRoute {
        self.route
    }

    /// Negotiated backend endpoint, suitable as a Hello-skip cache key.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Broker negotiation metadata, present when the broker path was used.
    pub fn negotiated(&self) -> Option<&Negotiated> {
        self.negotiated.as_ref()
    }

    /// Send one correlated request and await its response frame.
    pub async fn request(
        &mut self,
        payload_protocol: u32,
        payload: Vec<u8>,
    ) -> Result<Frame, FrameClientError> {
        self.client.request(payload_protocol, payload).await
    }

    /// Consume the session and return the owned async frame client.
    pub fn into_client(self) -> crate::broker::backend_sdk::AsyncFrameClient {
        self.client
    }
}
