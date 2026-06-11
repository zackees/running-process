//! Client-side helpers for the v1 broker Hello path.

use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use interprocess::local_socket::prelude::*;
use prost::Message;

use crate::broker::capabilities::{handoff_transport_available, CAP_HANDLE_PASSING};
use crate::broker::protocol::{
    hello_reply::Result as HelloReplyResult, read_frame, validate_frame_envelope, write_frame,
    AdminReply, AdminRequest, ErrorCode, Frame, FrameKind, FrameValidationError, FramingError,
    HandoffAck, Hello, HelloReply, Negotiated, PayloadEncoding, ADMIN_PAYLOAD_PROTOCOL,
    CONTROL_PAYLOAD_PROTOCOL, PROTOCOL_VERSION,
};
use crate::broker::server::handoff::validate_handoff_frame;
use crate::broker::server::local_socket_name;

/// Default wall-clock bound on waiting for the broker's handoff-ready relay
/// before silently downgrading to the `backend_pipe` reconnect path.
pub const DEFAULT_HANDOFF_READY_TIMEOUT: Duration = Duration::from_secs(2);

/// Canonical emergency escape hatch for participating broker consumers.
pub const RUNNING_PROCESS_DISABLE_ENV: &str = "RUNNING_PROCESS_DISABLE";
/// Value that disables broker usage and keeps the consumer on its direct path.
pub const RUNNING_PROCESS_DISABLE_VALUE: &str = "1";

/// Return whether the canonical broker escape hatch is enabled.
///
/// This helper only parses the shared environment contract. Consumers still
/// own the direct fallback path they should use when this returns `true`.
pub fn broker_disabled_by_env() -> Result<bool, BrokerDisableEnvError> {
    let Some(value) = std::env::var_os(RUNNING_PROCESS_DISABLE_ENV) else {
        return Ok(false);
    };
    let value = value.to_string_lossy();
    if value == RUNNING_PROCESS_DISABLE_VALUE {
        Ok(true)
    } else {
        Err(BrokerDisableEnvError {
            value: value.into_owned(),
        })
    }
}

/// Inputs for [`connect_to_backend`].
#[derive(Clone, Debug)]
pub struct ConnectBackendRequest<'a> {
    /// Broker pipe/socket endpoint.
    pub broker_endpoint: &'a str,
    /// Logical service name, such as `zccache`.
    pub service_name: &'a str,
    /// Backend version the caller wants.
    pub wanted_version: &'a str,
    /// Version of the caller's own service binary.
    pub self_version: &'a str,
    /// Previously negotiated backend endpoint, if the caller has one.
    pub cached_backend_endpoint: Option<&'a str>,
    /// Informational client version.
    pub client_version: &'a str,
    /// Client library name for diagnostics.
    pub client_lib_name: &'a str,
    /// Client library version for diagnostics.
    pub client_lib_version: &'a str,
    /// Proposed keepalive interval.
    pub client_keepalive_secs: u64,
    /// Opt in to adopting a handed-off backend connection (#354, slice 7).
    ///
    /// Default `false`: the client always reconnects to
    /// `Negotiated.backend_pipe`, exactly as before. When `true` AND the
    /// broker negotiated [`CAP_HANDLE_PASSING`] AND issued a non-empty
    /// `Negotiated.handle_passed_token`, the client waits up to
    /// [`Self::handoff_ready_timeout`] for the broker's handoff-ready relay
    /// (an EVENT frame under the `0xD0FF` handoff payload protocol carrying
    /// the backend's `HandoffAck`) on the SAME broker connection. On a valid
    /// accepted relay with a matching token echo, the client keeps that
    /// connection as the backend connection
    /// ([`BackendConnectionRoute::HandlePassed`]). Any failure — missing
    /// relay, timeout, refused or malformed ACK, token mismatch — silently
    /// downgrades to the `backend_pipe` reconnect; adoption failure is never
    /// an error by itself.
    pub adopt_handed_off_connection: bool,
    /// Deadline for the handoff-ready relay when
    /// [`Self::adopt_handed_off_connection`] is set.
    pub handoff_ready_timeout: Duration,
}

impl<'a> ConnectBackendRequest<'a> {
    /// Build a request with running-process defaults.
    pub fn new(
        broker_endpoint: &'a str,
        service_name: &'a str,
        wanted_version: &'a str,
        self_version: &'a str,
    ) -> Self {
        Self {
            broker_endpoint,
            service_name,
            wanted_version,
            self_version,
            cached_backend_endpoint: None,
            client_version: "",
            client_lib_name: "running-process",
            client_lib_version: env!("CARGO_PKG_VERSION"),
            client_keepalive_secs: 0,
            adopt_handed_off_connection: false,
            handoff_ready_timeout: DEFAULT_HANDOFF_READY_TIMEOUT,
        }
    }

    fn can_hello_skip(&self) -> bool {
        self.cached_backend_endpoint.is_some() && self.wanted_version == self.self_version
    }

    fn hello(&self) -> Hello {
        Hello {
            client_min_protocol: PROTOCOL_VERSION,
            client_max_protocol: PROTOCOL_VERSION,
            service_name: self.service_name.into(),
            wanted_version: self.wanted_version.into(),
            client_version: self.client_version.into(),
            client_capabilities: client_capabilities(),
            auth_token: Vec::new(),
            request_id: "hello".into(),
            connection_id: 0,
            peer_pid: std::process::id(),
            client_lib_name: self.client_lib_name.into(),
            client_lib_version: self.client_lib_version.into(),
            peer_attestation_nonce: Vec::new(),
            capability_token: Vec::new(),
            client_keepalive_secs: self.client_keepalive_secs,
        }
    }
}

/// Capability bitmap this client advertises in `Hello.client_capabilities`.
///
/// [`CAP_HANDLE_PASSING`] is advertised only when the build carries a
/// platform handoff transport (Windows `DuplicateHandle`, Unix
/// `SCM_RIGHTS`) — currently both, but kept explicit so an exotic target
/// degrades cleanly to the reconnect path.
fn client_capabilities() -> u64 {
    if handoff_transport_available() {
        CAP_HANDLE_PASSING
    } else {
        0
    }
}

/// How [`connect_to_backend`] reached the returned backend endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendConnectionRoute {
    /// Connected directly to a cached backend endpoint.
    HelloSkip,
    /// Asked the broker via Hello, then connected to the negotiated endpoint.
    BrokerNegotiated,
    /// Adopted the existing broker connection after a confirmed handoff.
    ///
    /// The broker handed the client's connection to the backend
    /// (`DuplicateHandle`/`SCM_RIGHTS`) and relayed the backend's accepted
    /// `HandoffAck` back to the client, so the socket that carried Hello is
    /// now served by the backend. No connection to `backend_pipe` was
    /// opened; [`BackendConnection::endpoint`] still reports the negotiated
    /// `backend_pipe` so callers can cache it for future Hello-skip.
    HandlePassed,
}

/// Open backend connection returned by [`connect_to_backend`].
#[derive(Debug)]
pub struct BackendConnection {
    /// Connected local socket stream.
    pub stream: interprocess::local_socket::Stream,
    /// Endpoint that was connected.
    ///
    /// For [`BackendConnectionRoute::HandlePassed`] this is the negotiated
    /// `backend_pipe` — useful as the Hello-skip cache key — even though the
    /// stream is the original broker connection rather than a fresh connect
    /// to that endpoint.
    pub endpoint: String,
    /// Route used to establish the connection.
    pub route: BackendConnectionRoute,
    /// Broker negotiation metadata when the broker path was used.
    pub negotiated: Option<Negotiated>,
}

impl BackendConnection {
    /// Pending one-time handoff token issued by the broker, if any.
    ///
    /// Non-empty only when both sides negotiated `CAP_HANDLE_PASSING`. By
    /// default the client still connects via `Negotiated.backend_pipe` and
    /// the route stays [`BackendConnectionRoute::BrokerNegotiated`]; when the
    /// caller opted in via
    /// [`ConnectBackendRequest::adopt_handed_off_connection`] and the broker
    /// confirmed the handoff, the route is
    /// [`BackendConnectionRoute::HandlePassed`] and this token is the one the
    /// confirmation echoed (#354).
    pub fn handoff_token(&self) -> Option<&[u8]> {
        self.negotiated
            .as_ref()
            .map(|negotiated| negotiated.handle_passed_token.as_slice())
            .filter(|token| !token.is_empty())
    }
}

/// Connect to a backend with the v1 Hello-skip fast path.
///
/// When `cached_backend_endpoint` is present and `wanted_version ==
/// self_version`, this tries the cached backend endpoint first. On miss,
/// or when the versions differ, it sends a broker `Hello`, reads the
/// broker `HelloReply`, and connects to `Negotiated.backend_pipe`.
///
/// With [`ConnectBackendRequest::adopt_handed_off_connection`] set and a
/// negotiated handoff (capability bit + non-empty token), the client first
/// waits — bounded by [`ConnectBackendRequest::handoff_ready_timeout`] — for
/// the broker's handoff-ready relay on the same connection and, when the
/// relay confirms the backend accepted, keeps that connection as the backend
/// connection. Any adoption failure silently falls back to the
/// `backend_pipe` reconnect below; reconnect remains the authoritative
/// correctness path.
pub fn connect_to_backend(
    request: ConnectBackendRequest<'_>,
) -> Result<BackendConnection, BrokerClientError> {
    if request.can_hello_skip() {
        if let Some(endpoint) = request.cached_backend_endpoint {
            if let Ok(stream) = connect_local_socket(endpoint) {
                return Ok(BackendConnection {
                    stream,
                    endpoint: endpoint.into(),
                    route: BackendConnectionRoute::HelloSkip,
                    negotiated: None,
                });
            }
        }
    }

    let (broker_stream, negotiated) = broker_hello(&request)?;
    if request.adopt_handed_off_connection && handoff_negotiated(&negotiated) {
        if let Some(adopted) = await_handoff_ready(
            broker_stream,
            negotiated.handle_passed_token.clone(),
            request.handoff_ready_timeout,
        ) {
            return Ok(BackendConnection {
                endpoint: negotiated.backend_pipe.clone(),
                stream: adopted,
                route: BackendConnectionRoute::HandlePassed,
                negotiated: Some(negotiated),
            });
        }
    }

    if negotiated.backend_pipe.is_empty() {
        return Err(BrokerClientError::EmptyBackendPipe);
    }
    let stream = connect_local_socket(&negotiated.backend_pipe)
        .map_err(BrokerClientError::BackendConnect)?;
    Ok(BackendConnection {
        endpoint: negotiated.backend_pipe.clone(),
        stream,
        route: BackendConnectionRoute::BrokerNegotiated,
        negotiated: Some(negotiated),
    })
}

/// True when the broker negotiated handle passing for this connection: the
/// server capability bit is set AND a one-time token was issued.
fn handoff_negotiated(negotiated: &Negotiated) -> bool {
    negotiated.server_capabilities & CAP_HANDLE_PASSING == CAP_HANDLE_PASSING
        && !negotiated.handle_passed_token.is_empty()
}

/// Wait (bounded) for the broker's handoff-ready relay on the Hello
/// connection and return the stream when adoption is confirmed.
///
/// The relay is an EVENT frame under the handoff payload protocol
/// (`0xD0FF`) whose payload is the backend's `HandoffAck`; the client
/// requires the token echo to match its negotiated one-time token and
/// `accepted = true`. The blocking framed read runs on a helper thread so
/// the wait is strictly deadline-bounded even though local-socket streams
/// have no portable read timeout; on timeout the stream stays with the
/// helper thread (which exits as soon as the abandoned read resolves) and
/// the caller falls back to reconnect. Every failure returns `None` —
/// adoption is best-effort by contract.
fn await_handoff_ready(
    stream: interprocess::local_socket::Stream,
    expected_token: Vec<u8>,
    timeout: Duration,
) -> Option<interprocess::local_socket::Stream> {
    let (result_tx, result_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut stream = stream;
        let outcome = read_handoff_ready(&mut stream, &expected_token).map(|()| stream);
        let _ = result_tx.send(outcome);
    });
    match result_rx.recv_timeout(timeout) {
        Ok(Ok(stream)) => Some(stream),
        Ok(Err(_)) | Err(_) => None,
    }
}

/// Read and validate one handoff-ready relay frame.
///
/// Errors carry a static description for diagnostics, but the adoption
/// contract maps every failure to the silent reconnect downgrade.
fn read_handoff_ready(
    stream: &mut interprocess::local_socket::Stream,
    expected_token: &[u8],
) -> Result<(), &'static str> {
    let bytes = read_frame(stream).map_err(|_| "failed to read handoff-ready frame")?;
    let frame =
        Frame::decode(bytes.as_slice()).map_err(|_| "failed to decode handoff-ready Frame")?;
    validate_handoff_frame(&frame, FrameKind::Event)?;
    let ack = HandoffAck::decode(frame.payload.as_slice())
        .map_err(|_| "failed to decode handoff-ready HandoffAck payload")?;
    if ack.token != expected_token {
        return Err("handoff-ready token echo does not match the negotiated token");
    }
    if !ack.accepted {
        return Err("broker relayed a refused handoff");
    }
    Ok(())
}

/// Send one typed admin request to a broker endpoint and return its reply.
pub fn send_admin_request(
    broker_endpoint: &str,
    request: AdminRequest,
) -> Result<AdminReply, BrokerClientError> {
    let mut stream =
        connect_local_socket(broker_endpoint).map_err(BrokerClientError::BrokerConnect)?;
    let request_frame = Frame {
        envelope_version: PROTOCOL_VERSION,
        kind: FrameKind::Request as i32,
        payload_protocol: ADMIN_PAYLOAD_PROTOCOL,
        payload: request.encode_to_vec(),
        request_id: 1,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    };
    write_frame(&mut stream, &request_frame.encode_to_vec())?;

    let response_bytes = read_frame(&mut stream)?;
    let response_frame =
        Frame::decode(response_bytes.as_slice()).map_err(BrokerClientError::DecodeFrame)?;
    validate_response_frame(
        &response_frame,
        ADMIN_PAYLOAD_PROTOCOL,
        "payload_protocol is not admin",
    )?;
    AdminReply::decode(response_frame.payload.as_slice())
        .map_err(BrokerClientError::DecodeAdminReply)
}

/// Open a platform local socket by broker endpoint string.
pub fn connect_local_socket(endpoint: &str) -> io::Result<interprocess::local_socket::Stream> {
    let name = local_socket_name(endpoint)?;
    LocalSocketStream::connect(name)
}

fn broker_hello(
    request: &ConnectBackendRequest<'_>,
) -> Result<(interprocess::local_socket::Stream, Negotiated), BrokerClientError> {
    let mut stream =
        connect_local_socket(request.broker_endpoint).map_err(BrokerClientError::BrokerConnect)?;
    let hello = request.hello();
    let request_frame = Frame {
        envelope_version: PROTOCOL_VERSION,
        kind: FrameKind::Request as i32,
        payload_protocol: CONTROL_PAYLOAD_PROTOCOL,
        payload: hello.encode_to_vec(),
        request_id: 1,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    };
    write_frame(&mut stream, &request_frame.encode_to_vec())?;

    let response_bytes = read_frame(&mut stream)?;
    let response_frame =
        Frame::decode(response_bytes.as_slice()).map_err(BrokerClientError::DecodeFrame)?;
    validate_response_frame(
        &response_frame,
        CONTROL_PAYLOAD_PROTOCOL,
        "payload_protocol is not control-plane",
    )?;
    let reply = HelloReply::decode(response_frame.payload.as_slice())
        .map_err(BrokerClientError::DecodeHelloReply)?;
    match reply
        .result
        .ok_or(BrokerClientError::MissingHelloReplyResult)?
    {
        HelloReplyResult::Negotiated(negotiated) => Ok((stream, negotiated)),
        HelloReplyResult::Refused(refused) => Err(BrokerClientError::Refused {
            code: ErrorCode::try_from(refused.code).unwrap_or(ErrorCode::Unspecified),
            reason: refused.reason,
            retry_after_ms: refused.retry_after_ms,
        }),
    }
}

fn validate_response_frame(
    frame: &Frame,
    expected_payload_protocol: u32,
    payload_protocol_error: &'static str,
) -> Result<(), BrokerClientError> {
    validate_frame_envelope(frame, FrameKind::Response, expected_payload_protocol).map_err(
        |error| {
            BrokerClientError::UnexpectedResponseFrame(match error {
                FrameValidationError::EnvelopeVersion { .. } => "envelope_version is not v1",
                FrameValidationError::Kind { .. } => "kind is not RESPONSE",
                FrameValidationError::PayloadProtocol { .. } => payload_protocol_error,
                FrameValidationError::PayloadEncoding { .. } => "payload is compressed",
            })
        },
    )
}

/// Invalid value for the canonical broker disable variable.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("RUNNING_PROCESS_DISABLE must be unset or 1, got {value:?}")]
pub struct BrokerDisableEnvError {
    /// Value read from `RUNNING_PROCESS_DISABLE`.
    pub value: String,
}

/// Errors produced by broker client helpers.
#[derive(Debug, thiserror::Error)]
pub enum BrokerClientError {
    /// Could not connect to the broker.
    #[error("failed to connect to broker: {0}")]
    BrokerConnect(io::Error),
    /// Broker negotiation succeeded but the returned backend endpoint failed.
    #[error("failed to connect to negotiated backend: {0}")]
    BackendConnect(io::Error),
    /// Frame read/write failed.
    #[error(transparent)]
    Framing(#[from] FramingError),
    /// Broker response frame was malformed.
    #[error("failed to decode broker response Frame: {0}")]
    DecodeFrame(prost::DecodeError),
    /// Broker response payload was not a valid `HelloReply`.
    #[error("failed to decode broker HelloReply: {0}")]
    DecodeHelloReply(prost::DecodeError),
    /// Broker response payload was not a valid `AdminReply`.
    #[error("failed to decode broker AdminReply: {0}")]
    DecodeAdminReply(prost::DecodeError),
    /// Broker returned an unexpected response envelope.
    #[error("unexpected broker response frame: {0}")]
    UnexpectedResponseFrame(&'static str),
    /// Broker returned `HelloReply` without a result.
    #[error("broker HelloReply did not contain a result")]
    MissingHelloReplyResult,
    /// Broker refused the Hello request.
    #[error("broker refused Hello: {reason} ({code:?}, retry_after_ms={retry_after_ms})")]
    Refused {
        /// Stable refusal code.
        code: ErrorCode,
        /// Human-readable reason.
        reason: String,
        /// Retry hint.
        retry_after_ms: u64,
    },
    /// Broker returned an empty backend endpoint.
    #[error("broker negotiated an empty backend endpoint")]
    EmptyBackendPipe,
}
