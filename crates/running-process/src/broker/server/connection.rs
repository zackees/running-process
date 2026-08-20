//! Framed broker connection handling for the v1 Hello path.
//!
//! This module keeps the wire I/O boundary separate from
//! [`HelloHandler`]. The long-lived accept loop can call the same
//! single-connection function after binding the platform pipe/socket and
//! verifying peer credentials.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::thread;

use prost::Message;

use crate::broker::protocol::{
    hello_reply::Result as HelloReplyResult, read_frame_with_cap, write_frame, ErrorCode, Frame,
    FrameKind, FramingError, HelloReply, PayloadEncoding, Refused, CONTROL_PAYLOAD_PROTOCOL,
    MAX_HELLO_BYTES, PROTOCOL_VERSION,
};
use crate::broker::server::deadline_stream::{hello_read_deadline, with_nonblocking_deadline};
use crate::broker::server::{HelloHandler, HelloRouter, PeerIdentity};
use crate::platform::ipc;

/// Peer credential policy applied before reading a Hello frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerCredentialPolicy {
    /// Accept any peer whose platform credentials can be read.
    AllowAny,
    /// Accept only peers whose UID or SID exactly matches `uid_or_sid`.
    OwnerOnly {
        /// Expected owner UID or SID string.
        uid_or_sid: String,
    },
}

impl PeerCredentialPolicy {
    /// Build a permissive policy.
    pub fn allow_any() -> Self {
        Self::AllowAny
    }

    /// Build a policy that accepts only one owner UID or SID.
    pub fn owner_only(uid_or_sid: impl Into<String>) -> Self {
        Self::OwnerOnly {
            uid_or_sid: uid_or_sid.into(),
        }
    }

    /// Build an owner-only policy for the current platform user.
    pub fn current_user() -> Option<Self> {
        ipc::current_user_id().ok().map(Self::owner_only)
    }

    /// Return true when `peer` is authorized by this policy.
    pub fn allows(&self, peer: &PeerIdentity) -> bool {
        match self {
            Self::AllowAny => true,
            Self::OwnerOnly { uid_or_sid } => {
                !uid_or_sid.is_empty() && peer.uid_or_sid == *uid_or_sid
            }
        }
    }
}

/// Handles a decoded broker Hello frame and returns the protocol reply.
///
/// This keeps the frame I/O boundary independent from the concrete routing
/// strategy. Tests and preloaded-backend serve mode can use [`HelloHandler`], while the
/// broker accept loop can route through [`HelloRouter`].
pub trait HelloResponder {
    /// Decode and answer a broker Hello frame for an OS-verified peer.
    fn handle_frame(&self, frame: Frame, peer: PeerIdentity) -> HelloReply;
}

impl HelloResponder for HelloHandler {
    fn handle_frame(&self, frame: Frame, peer: PeerIdentity) -> HelloReply {
        Self::handle_frame(self, frame, peer)
    }
}

impl HelloResponder for HelloRouter<'_> {
    fn handle_frame(&self, frame: Frame, peer: PeerIdentity) -> HelloReply {
        Self::handle_frame(self, frame, peer)
    }
}

/// Handle one already-accepted broker connection.
///
/// The connection reads exactly one v1-framed [`Frame`], decodes the
/// embedded `Hello`, writes one v1-framed response [`Frame`] containing
/// a `HelloReply`, then returns the reply for metrics/logging callers.
pub fn handle_hello_connection<S: Read + Write>(
    stream: &mut S,
    handler: &HelloHandler,
    peer: PeerIdentity,
) -> Result<HelloReply, BrokerConnectionError> {
    handle_hello_connection_with(stream, handler, peer)
}

/// Handle one already-accepted broker connection with a pluggable responder.
///
/// The framed wire behavior is identical to [`handle_hello_connection`]; only
/// the decoded Hello routing strategy is supplied by the caller.
pub fn handle_hello_connection_with<S, R>(
    stream: &mut S,
    responder: &R,
    peer: PeerIdentity,
) -> Result<HelloReply, BrokerConnectionError>
where
    S: Read + Write,
    R: HelloResponder + ?Sized,
{
    handle_hello_connection_with_peer_policy(
        stream,
        responder,
        peer,
        &PeerCredentialPolicy::allow_any(),
    )
    .map(|reply| reply.expect("allow-any policy must not drop peers"))
}

/// Handle one already-accepted broker connection with an explicit peer policy.
///
/// Returns `Ok(None)` when the policy rejects the peer. The caller should drop
/// the stream without writing a `HelloReply`; this is the broker's silent
/// foreign-peer rejection path.
pub fn handle_hello_connection_with_peer_policy<S, R>(
    stream: &mut S,
    responder: &R,
    peer: PeerIdentity,
    peer_policy: &PeerCredentialPolicy,
) -> Result<Option<HelloReply>, BrokerConnectionError>
where
    S: Read + Write,
    R: HelloResponder + ?Sized,
{
    if !peer_policy.allows(&peer) {
        return Ok(None);
    }

    let request_bytes = match read_frame_with_cap(stream, MAX_HELLO_BYTES) {
        Ok(bytes) => bytes,
        Err(err) => {
            let reply = reply_for_framing_error(&err);
            write_response_frame(stream, None, &reply)?;
            return Ok(Some(reply));
        }
    };

    let request_frame = match Frame::decode(request_bytes.as_slice()) {
        Ok(frame) => frame,
        Err(_) => {
            let reply = refused_reply(ErrorCode::ErrorPeerRejected, "malformed broker Frame", 0);
            write_response_frame(stream, None, &reply)?;
            return Ok(Some(reply));
        }
    };

    let reply = responder.handle_frame(request_frame.clone(), peer);
    write_response_frame(stream, Some(&request_frame), &reply)?;
    Ok(Some(reply))
}

/// Run one blocking local-socket accept and serve exactly one Hello.
///
/// This is a testable stepping stone toward the full Phase 4 accept
/// loop. It binds the platform local socket, accepts one peer, derives
/// available OS peer credentials, serves one framed Hello exchange, and
/// returns.
pub fn serve_one_local_socket(
    socket_path: &str,
    handler: &HelloHandler,
) -> Result<HelloReply, BrokerConnectionError> {
    serve_one_local_socket_with(socket_path, handler)
}

/// Run one blocking local-socket accept and serve exactly one Hello with a
/// pluggable responder.
pub fn serve_one_local_socket_with<R>(
    socket_path: &str,
    responder: &R,
) -> Result<HelloReply, BrokerConnectionError>
where
    R: HelloResponder + ?Sized,
{
    serve_one_local_socket_with_peer_policy(
        socket_path,
        responder,
        &PeerCredentialPolicy::allow_any(),
    )
    .map(|reply| reply.expect("allow-any policy must not drop peers"))
}

/// Run one blocking local-socket accept with an explicit peer policy.
pub fn serve_one_local_socket_with_peer_policy<R>(
    socket_path: &str,
    responder: &R,
    peer_policy: &PeerCredentialPolicy,
) -> Result<Option<HelloReply>, BrokerConnectionError>
where
    R: HelloResponder + ?Sized,
{
    let listener = bind_local_socket(socket_path)?;
    let cleanup = LocalSocketCleanup(socket_path);
    let result = (|| {
        let mut stream = listener.accept()?;
        let peer = peer_identity_from_stream(&stream)?;
        with_nonblocking_deadline(&mut stream, hello_read_deadline(), |stream| {
            handle_hello_connection_with_peer_policy(stream, responder, peer, peer_policy)
        })
    })();
    drop(listener);
    drop(cleanup);
    result
}

/// Run a bounded blocking local-socket accept loop.
///
/// This is the synchronous Phase 4 test harness for the Hello accept
/// path. It accepts `connection_count` peers, handles each connection
/// on a worker thread, waits for all workers, then returns.
pub fn serve_local_socket_connections(
    socket_path: &str,
    handler: Arc<HelloHandler>,
    connection_count: usize,
) -> Result<(), BrokerConnectionError> {
    serve_local_socket_connections_with_peer_policy(
        socket_path,
        handler,
        connection_count,
        &PeerCredentialPolicy::allow_any(),
    )
}

/// Run a bounded blocking local-socket accept loop with an explicit peer policy.
pub fn serve_local_socket_connections_with_peer_policy(
    socket_path: &str,
    handler: Arc<HelloHandler>,
    connection_count: usize,
    peer_policy: &PeerCredentialPolicy,
) -> Result<(), BrokerConnectionError> {
    if connection_count == 0 {
        return Ok(());
    }

    let listener = bind_local_socket(socket_path)?;
    let cleanup = LocalSocketCleanup(socket_path);
    let result = (|| {
        let mut workers = Vec::with_capacity(connection_count);
        let peer_policy = Arc::new(peer_policy.clone());

        for _ in 0..connection_count {
            let mut stream = listener.accept()?;
            let peer = peer_identity_from_stream(&stream)?;
            let handler = Arc::clone(&handler);
            let peer_policy = Arc::clone(&peer_policy);
            workers.push(thread::spawn(move || {
                with_nonblocking_deadline(&mut stream, hello_read_deadline(), |stream| {
                    handle_hello_connection_with_peer_policy(
                        stream,
                        handler.as_ref(),
                        peer,
                        peer_policy.as_ref(),
                    )
                    .map(|_| ())
                })
            }));
        }

        for worker in workers {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(err)) => return Err(err),
                Err(_) => return Err(BrokerConnectionError::WorkerPanic),
            }
        }
        Ok(())
    })();
    drop(listener);
    drop(cleanup);
    result
}

/// Run a bounded blocking local-socket accept loop with a pluggable responder.
///
/// This serves accepted connections sequentially so responders may borrow
/// broker-owned state that is not safe to share across worker threads, such as
/// platform process handles in the backend registry.
pub fn serve_local_socket_connections_with<R>(
    socket_path: &str,
    responder: &R,
    connection_count: usize,
) -> Result<(), BrokerConnectionError>
where
    R: HelloResponder + ?Sized,
{
    serve_local_socket_connections_with_policy(
        socket_path,
        responder,
        connection_count,
        &PeerCredentialPolicy::allow_any(),
    )
}

/// Run a bounded pluggable-responder accept loop with an explicit peer policy.
pub fn serve_local_socket_connections_with_policy<R>(
    socket_path: &str,
    responder: &R,
    connection_count: usize,
    peer_policy: &PeerCredentialPolicy,
) -> Result<(), BrokerConnectionError>
where
    R: HelloResponder + ?Sized,
{
    if connection_count == 0 {
        return Ok(());
    }

    let listener = bind_local_socket(socket_path)?;
    let cleanup = LocalSocketCleanup(socket_path);
    let result = (|| {
        for _ in 0..connection_count {
            let mut stream = listener.accept()?;
            let peer = peer_identity_from_stream(&stream)?;
            let _ = with_nonblocking_deadline(&mut stream, hello_read_deadline(), |stream| {
                handle_hello_connection_with_peer_policy(stream, responder, peer, peer_policy)
            })?;
        }
        Ok(())
    })();
    drop(listener);
    drop(cleanup);
    result
}

/// Convert the broker's platform socket path/name string into an
/// `interprocess` local-socket name.
pub fn local_socket_name(socket_path: &str) -> io::Result<interprocess::local_socket::Name<'_>> {
    super::singleton_bind::wrap_socket_name(socket_path).map_err(io::Error::other)
}

/// Errors raised while serving a framed broker Hello connection.
#[derive(Debug, thiserror::Error)]
pub enum BrokerConnectionError {
    /// v1 framing failed.
    #[error(transparent)]
    Framing(#[from] FramingError),
    /// The response frame could not be encoded.
    #[error("failed to encode broker response Frame: {0}")]
    EncodeFrame(prost::EncodeError),
    /// Local socket I/O failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A connection worker thread panicked.
    #[error("broker connection worker panicked")]
    WorkerPanic,
}

pub(super) fn bind_local_socket(socket_path: &str) -> Result<ipc::Listener, BrokerConnectionError> {
    let endpoint = ipc::Endpoint::new(socket_path.to_owned())?;
    prepare_local_socket_path(&endpoint)?;
    let listener = ipc::Listener::bind_owner_only(&endpoint)?;
    Ok(listener)
}

pub(super) struct LocalSocketCleanup<'a>(pub(super) &'a str);

impl Drop for LocalSocketCleanup<'_> {
    fn drop(&mut self) {
        cleanup_local_socket_path(self.0);
    }
}

pub(super) fn write_response_frame<W: Write>(
    writer: &mut W,
    request_frame: Option<&Frame>,
    reply: &HelloReply,
) -> Result<(), BrokerConnectionError> {
    let response_frame = Frame {
        envelope_version: PROTOCOL_VERSION,
        kind: FrameKind::Response as i32,
        payload_protocol: CONTROL_PAYLOAD_PROTOCOL,
        payload: reply.encode_to_vec(),
        request_id: request_frame.map_or(0, |frame| frame.request_id),
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: request_frame
            .map(|frame| frame.traceparent.clone())
            .unwrap_or_default(),
        tracestate: request_frame
            .map(|frame| frame.tracestate.clone())
            .unwrap_or_default(),
    };
    let mut response_bytes = Vec::new();
    response_frame
        .encode(&mut response_bytes)
        .map_err(BrokerConnectionError::EncodeFrame)?;
    write_frame(writer, &response_bytes)?;
    Ok(())
}

pub(super) fn reply_for_framing_error(error: &FramingError) -> HelloReply {
    match error {
        FramingError::UnsupportedFramingVersion { .. } => refused_reply(
            ErrorCode::ErrorVersionUnsupported,
            "unsupported framing version",
            0,
        ),
        FramingError::FrameTooLarge { .. } => refused_reply(
            ErrorCode::ErrorPeerRejected,
            "initial Hello frame exceeds 64 KiB",
            0,
        ),
        FramingError::UnexpectedEof { .. } | FramingError::Io(_) => {
            refused_reply(ErrorCode::ErrorPeerRejected, "incomplete Hello frame", 0)
        }
        // Buffer-level codec variant (#412); the stream-level reads used
        // here never produce it, but a malformed body is a peer fault.
        FramingError::Decode(_) => {
            refused_reply(ErrorCode::ErrorPeerRejected, "malformed Hello frame", 0)
        }
    }
}

pub(super) fn refused_reply(
    code: ErrorCode,
    reason: impl Into<String>,
    retry_after_ms: u64,
) -> HelloReply {
    HelloReply {
        result: Some(HelloReplyResult::Refused(Refused {
            reason: reason.into(),
            daemon_min_protocol: PROTOCOL_VERSION,
            daemon_max_protocol: PROTOCOL_VERSION,
            code: code as i32,
            details: HashMap::new(),
            retry_after_ms,
        })),
    }
}

pub fn peer_identity_from_stream<S>(stream: &S) -> Result<PeerIdentity, BrokerConnectionError>
where
    S: ipc::PeerIdentitySource + ?Sized,
{
    peer_identity_from_source(stream)
}

/// Async analog of [`peer_identity_from_stream`] for tokio-`interprocess`
/// streams (soldr#2365, the async v2 broker serve path).
///
/// `peer_creds()` is a synchronous `getsockopt`/`GetNamedPipeClientProcessId`
/// query — it does not block on I/O — so it is safe to call from an async
/// accept loop without yielding. The credential extraction is identical to the
/// sync path (shared `peer_identity_from_peer_creds`).
#[cfg(feature = "client-async")]
pub fn peer_identity_from_tokio_stream<S>(stream: &S) -> Result<PeerIdentity, BrokerConnectionError>
where
    S: ipc::PeerIdentitySource + ?Sized,
{
    peer_identity_from_source(stream)
}

fn peer_identity_from_source<S>(stream: &S) -> Result<PeerIdentity, BrokerConnectionError>
where
    S: ipc::PeerIdentitySource + ?Sized,
{
    let peer = ipc::PeerIdentitySource::ipc_peer_identity(stream)?;
    Ok(PeerIdentity {
        pid: peer.pid,
        uid_or_sid: peer.user_id,
    })
}

fn prepare_local_socket_path(endpoint: &ipc::Endpoint) -> io::Result<()> {
    endpoint.ensure_owner_private_parent()?;
    if endpoint.target_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "broker local socket path already exists",
        ));
    }
    Ok(())
}

fn cleanup_local_socket_path(socket_path: &str) {
    if let Ok(endpoint) = ipc::Endpoint::new(socket_path.to_owned()) {
        let _ = endpoint.retire();
    }
}

#[cfg(test)]
mod endpoint_tests {
    use super::local_socket_name;

    #[test]
    fn resolved_pipe_has_one_canonical_interprocess_name() {
        use crate::broker::server::singleton_bind::{resolve_socket_path, wrap_socket_name};

        let resolved = resolve_socket_path(&format!(
            "rpb-v2-canonical-name-test-{}",
            std::process::id()
        ))
        .expect("resolve Windows pipe path");
        let server = local_socket_name(&resolved).expect("server name");
        let client = wrap_socket_name(&resolved).expect("client name");

        assert_eq!(
            server, client,
            "server bind and client dial must normalize a resolved pipe identically"
        );
    }
}
