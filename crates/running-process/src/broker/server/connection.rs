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

use interprocess::local_socket::traits::Listener;
use prost::Message;

use crate::broker::protocol::{
    hello_reply::Result as HelloReplyResult, read_frame_with_cap, write_frame, ErrorCode, Frame,
    FrameKind, FramingError, HelloReply, PayloadEncoding, Refused, MAX_HELLO_BYTES,
};
use crate::broker::server::{HelloHandler, HelloRouter, PeerIdentity};

const PROTOCOL_VERSION: u32 = 1;
const CONTROL_PAYLOAD_PROTOCOL: u32 = 0;

/// Handles a decoded broker Hello frame and returns the protocol reply.
///
/// This keeps the frame I/O boundary independent from the concrete routing
/// strategy. Tests and bounded serve mode can use [`HelloHandler`], while the
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
    let request_bytes = match read_frame_with_cap(stream, MAX_HELLO_BYTES) {
        Ok(bytes) => bytes,
        Err(err) => {
            let reply = reply_for_framing_error(&err);
            write_response_frame(stream, None, &reply)?;
            return Ok(reply);
        }
    };

    let request_frame = match Frame::decode(request_bytes.as_slice()) {
        Ok(frame) => frame,
        Err(_) => {
            let reply = refused_reply(
                ErrorCode::ErrorPeerRejected,
                "malformed broker Frame",
                0,
            );
            write_response_frame(stream, None, &reply)?;
            return Ok(reply);
        }
    };

    let reply = responder.handle_frame(request_frame.clone(), peer);
    write_response_frame(stream, Some(&request_frame), &reply)?;
    Ok(reply)
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
    let listener = bind_local_socket(socket_path)?;
    let _cleanup = LocalSocketCleanup(socket_path);

    let mut stream = listener.accept()?;
    let peer = peer_identity_from_stream(&stream)?;
    handle_hello_connection_with(&mut stream, responder, peer)
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
    if connection_count == 0 {
        return Ok(());
    }

    let listener = bind_local_socket(socket_path)?;
    let _cleanup = LocalSocketCleanup(socket_path);
    let mut workers = Vec::with_capacity(connection_count);

    for _ in 0..connection_count {
        let mut stream = listener.accept()?;
        let peer = peer_identity_from_stream(&stream)?;
        let handler = Arc::clone(&handler);
        workers.push(thread::spawn(move || {
            handle_hello_connection(&mut stream, handler.as_ref(), peer).map(|_| ())
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
    if connection_count == 0 {
        return Ok(());
    }

    let listener = bind_local_socket(socket_path)?;
    let _cleanup = LocalSocketCleanup(socket_path);

    for _ in 0..connection_count {
        let mut stream = listener.accept()?;
        let peer = peer_identity_from_stream(&stream)?;
        handle_hello_connection_with(&mut stream, responder, peer)?;
    }
    Ok(())
}

/// Convert the broker's platform socket path/name string into an
/// `interprocess` local-socket name.
pub fn local_socket_name(
    socket_path: &str,
) -> io::Result<interprocess::local_socket::Name<'_>> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        socket_path.to_fs_name::<GenericFilePath>()
    }

    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};
        socket_path.to_ns_name::<GenericNamespaced>()
    }
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

fn bind_local_socket(
    socket_path: &str,
) -> Result<interprocess::local_socket::Listener, BrokerConnectionError> {
    use interprocess::local_socket::ListenerOptions;

    prepare_local_socket_path(socket_path)?;
    let name = local_socket_name(socket_path)?;
    let listener = ListenerOptions::new().name(name).create_sync()?;
    secure_local_socket_path(socket_path)?;
    Ok(listener)
}

struct LocalSocketCleanup<'a>(&'a str);

impl Drop for LocalSocketCleanup<'_> {
    fn drop(&mut self) {
        cleanup_local_socket_path(self.0);
    }
}

fn write_response_frame<W: Write>(
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

fn reply_for_framing_error(error: &FramingError) -> HelloReply {
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
    }
}

fn refused_reply(code: ErrorCode, reason: impl Into<String>, retry_after_ms: u64) -> HelloReply {
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

fn peer_identity_from_stream(
    stream: &interprocess::local_socket::Stream,
) -> Result<PeerIdentity, BrokerConnectionError> {
    use interprocess::local_socket::traits::StreamCommon;

    let creds = stream.peer_creds()?;
    #[cfg(unix)]
    let pid = creds
        .pid()
        .and_then(|pid| u32::try_from(pid).ok())
        .unwrap_or(0);

    #[cfg(windows)]
    let pid = creds.pid().unwrap_or(0);

    #[cfg(unix)]
    let uid_or_sid = creds.euid().map(|uid| uid.to_string()).unwrap_or_default();

    #[cfg(windows)]
    let uid_or_sid = String::new();

    Ok(PeerIdentity { pid, uid_or_sid })
}

fn prepare_local_socket_path(socket_path: &str) -> io::Result<()> {
    #[cfg(unix)]
    {
        let path = std::path::Path::new(socket_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(path);
    }

    #[cfg(windows)]
    let _ = socket_path;

    Ok(())
}

fn secure_local_socket_path(socket_path: &str) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(socket_path, perms)?;
    }

    #[cfg(windows)]
    let _ = socket_path;

    Ok(())
}

fn cleanup_local_socket_path(socket_path: &str) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(socket_path);
    }

    #[cfg(windows)]
    let _ = socket_path;
}
