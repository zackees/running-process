//! Shared broker control socket dispatch for Hello and admin frames.
//!
//! The v1 broker uses one local socket for both client Hello negotiation and
//! admin verbs. This module keeps the bounded synchronous serve helpers aligned
//! with that contract while the long-lived daemon loop is still being built.

use std::io::{Read, Write};
use std::num::NonZeroUsize;

use interprocess::local_socket::traits::Listener;
use prost::Message;

use crate::broker::protocol::{
    read_frame, write_frame, AdminReply, ErrorCode, Frame, FramingError, HelloReply,
    MAX_HELLO_BYTES,
};

use super::admin::{handle_admin_frame, AdminFrameError, AdminSnapshot, ADMIN_PAYLOAD_PROTOCOL};
use super::connection::{
    bind_local_socket, peer_identity_from_stream, refused_reply, reply_for_framing_error,
    write_response_frame, BrokerConnectionError, HelloResponder, LocalSocketCleanup,
    PeerCredentialPolicy,
};
use super::hello_handler::PeerIdentity;

/// Result of handling one control socket connection.
#[derive(Clone, Debug, PartialEq)]
pub enum ControlSocketReply {
    /// Peer was rejected by credential policy before any bytes were read.
    DroppedPeer,
    /// The connection was handled as a Hello exchange.
    Hello(HelloReply),
    /// The connection was handled as an admin request.
    Admin(AdminReply),
}

/// Connection limit for a broker control-socket accept loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlSocketConnectionLimit {
    /// Accept exactly this many connections, then return.
    Bounded(NonZeroUsize),
    /// Continue accepting until the process exits or binding/accepting fails.
    Unbounded,
}

impl ControlSocketConnectionLimit {
    fn should_continue(self, accepted: usize) -> bool {
        match self {
            Self::Bounded(limit) => accepted < limit.get(),
            Self::Unbounded => true,
        }
    }
}

/// Handle one already-accepted broker control connection.
pub fn handle_control_connection_with_peer_policy<S, R, F>(
    stream: &mut S,
    hello_responder: &R,
    snapshot_provider: &F,
    peer: PeerIdentity,
    peer_policy: &PeerCredentialPolicy,
) -> Result<ControlSocketReply, ControlSocketError>
where
    S: Read + Write,
    R: HelloResponder + ?Sized,
    F: Fn() -> AdminSnapshot + ?Sized,
{
    if !peer_policy.allows(&peer) {
        return Ok(ControlSocketReply::DroppedPeer);
    }

    let request_bytes = match read_frame(stream) {
        Ok(bytes) => bytes,
        Err(err) => {
            let reply = reply_for_framing_error(&err);
            write_response_frame(stream, None, &reply)?;
            return Ok(ControlSocketReply::Hello(reply));
        }
    };

    let request_frame = match Frame::decode(request_bytes.as_slice()) {
        Ok(frame) => frame,
        Err(_) => {
            let reply = refused_reply(ErrorCode::ErrorPeerRejected, "malformed broker Frame", 0);
            write_response_frame(stream, None, &reply)?;
            return Ok(ControlSocketReply::Hello(reply));
        }
    };

    if request_frame.payload_protocol == ADMIN_PAYLOAD_PROTOCOL {
        let snapshot = snapshot_provider();
        let response_frame = handle_admin_frame(request_frame, &snapshot)?;
        let reply = write_admin_response_frame(stream, &response_frame)?;
        return Ok(ControlSocketReply::Admin(reply));
    }

    let reply = if request_bytes.len() > MAX_HELLO_BYTES {
        refused_reply(
            ErrorCode::ErrorPeerRejected,
            "initial Hello frame exceeds 64 KiB",
            0,
        )
    } else {
        hello_responder.handle_frame(request_frame.clone(), peer)
    };
    write_response_frame(stream, Some(&request_frame), &reply)?;
    Ok(ControlSocketReply::Hello(reply))
}

/// Run a bounded local-socket accept loop that dispatches Hello and admin
/// frames on the same endpoint.
pub fn serve_control_socket_connections_with_policy<R, F>(
    socket_path: &str,
    hello_responder: &R,
    snapshot_provider: F,
    connection_count: usize,
    peer_policy: &PeerCredentialPolicy,
) -> Result<(), ControlSocketError>
where
    R: HelloResponder + ?Sized,
    F: Fn() -> AdminSnapshot,
{
    let Some(connection_count) = NonZeroUsize::new(connection_count) else {
        return Ok(());
    };

    serve_control_socket_connections_with_limit_and_policy(
        socket_path,
        hello_responder,
        snapshot_provider,
        ControlSocketConnectionLimit::Bounded(connection_count),
        peer_policy,
    )
}

/// Run a broker control-socket accept loop that dispatches Hello and admin
/// frames on the same endpoint.
pub fn serve_control_socket_connections_with_limit_and_policy<R, F>(
    socket_path: &str,
    hello_responder: &R,
    snapshot_provider: F,
    connection_limit: ControlSocketConnectionLimit,
    peer_policy: &PeerCredentialPolicy,
) -> Result<(), ControlSocketError>
where
    R: HelloResponder + ?Sized,
    F: Fn() -> AdminSnapshot,
{
    let listener = bind_local_socket(socket_path)?;
    let cleanup = LocalSocketCleanup(socket_path);
    let result = (|| {
        let mut accepted = 0;
        while connection_limit.should_continue(accepted) {
            let mut stream = listener.accept().map_err(BrokerConnectionError::Io)?;
            accepted += 1;
            let peer = peer_identity_from_stream(&stream)?;
            let _reply = handle_control_connection_with_peer_policy(
                &mut stream,
                hello_responder,
                &snapshot_provider,
                peer,
                peer_policy,
            )?;
        }
        Ok(())
    })();
    drop(listener);
    drop(cleanup);
    result
}

fn write_admin_response_frame<W: Write>(
    writer: &mut W,
    response_frame: &Frame,
) -> Result<AdminReply, ControlSocketError> {
    let mut response_bytes = Vec::new();
    response_frame
        .encode(&mut response_bytes)
        .map_err(ControlSocketError::EncodeFrame)?;
    write_frame(writer, &response_bytes)?;
    AdminReply::decode(response_frame.payload.as_slice())
        .map_err(ControlSocketError::DecodeAdminReply)
}

/// Errors raised while dispatching a shared broker control socket frame.
#[derive(Debug, thiserror::Error)]
pub enum ControlSocketError {
    /// Hello/local-socket connection handling failed.
    #[error(transparent)]
    Connection(#[from] BrokerConnectionError),
    /// Frame read/write failed.
    #[error(transparent)]
    Framing(#[from] FramingError),
    /// Admin frame validation or dispatch failed.
    #[error(transparent)]
    AdminFrame(#[from] AdminFrameError),
    /// The response frame could not be encoded.
    #[error("failed to encode broker control response Frame: {0}")]
    EncodeFrame(prost::EncodeError),
    /// The admin response payload could not be decoded after dispatch.
    #[error("failed to decode admin reply payload: {0}")]
    DecodeAdminReply(prost::DecodeError),
}
