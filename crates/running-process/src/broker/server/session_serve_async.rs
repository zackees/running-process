//! Async v2 broker SESSION serve path — the strangler-fig async twin of the
//! synchronous control-socket loop (soldr#2365).
//!
//! The owner-directed async reversal: the v2 broker's SESSION data plane is
//! full-proxy (client → broker → daemon, every byte relayed), which needs
//! [`tokio::io::copy_bidirectional`] and a tokio-`interprocess` accept loop.
//! Rather than flip the whole sync serve stack to `async fn` at once, this
//! module adds a NEW async entry that **reuses the identical sync decision
//! core** — the [`HelloResponder`] (`hello_router::HelloRouter` in production)
//! is called exactly as the sync path calls it; only the socket I/O is async.
//!
//! Flow, mirroring the sync `handle_control_connection` + `post_hello` relay:
//!
//! 1. read exactly one `[1][u32 LE len][Frame]` Hello frame — **manually**, so
//!    no read buffer over-reads into the client's first SESSION bytes (that
//!    `SessionStart` must reach the daemon's `serve_session` intact);
//! 2. route it through the sync `responder` to a [`HelloReply`];
//! 3. write the framed reply back;
//! 4. on a negotiated SESSION, hand the **same** connection to
//!    [`relay_session`](crate::broker::session_relay::relay_session), which
//!    dials the daemon SESSION endpoint and proxies bytes both ways.
//!
//! # Not yet wired to the binary
//!
//! Peer-credential extraction is unresolved on tokio-`interprocess` streams
//! (`peer_creds()` is a sync-`Stream` method). This module uses a placeholder
//! [`PeerIdentity`] and therefore does **not** enforce
//! [`PeerCredentialPolicy`](super::connection::PeerCredentialPolicy) — the sync
//! path refuses foreign peers, so wiring this into
//! `running-process-broker-v2` is BLOCKED until async peer creds land. This is
//! a proven spike of the transport, not a production serve loop.

use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::broker::protocol::{
    hello_reply::Result as HelloReplyResult, ErrorCode, Frame, FrameKind, HelloReply, Negotiated,
    PayloadEncoding, CONTROL_PAYLOAD_PROTOCOL, ENVELOPE_VERSION, MAX_HELLO_BYTES, PROTOCOL_VERSION,
};
use crate::broker::session_relay::relay_session;

use super::connection::refused_reply;
use super::connection::HelloResponder;
use super::hello_handler::PeerIdentity;

/// Accept SESSION connections on `listener`, negotiate each Hello with the sync
/// `responder`, and full-proxy every negotiated session to the daemon SESSION
/// endpoint at `daemon_session_endpoint`.
///
/// The Hello round-trip runs inline (it borrows `responder`, which holds
/// broker-owned non-`Send` routing state, exactly as the sync loop does), so
/// negotiation is sequential and cheap; the byte relay — the only long-lived
/// part — is spawned per connection so concurrent compiles never serialize.
///
/// # Errors
///
/// Returns the first fatal `accept()` error (the listener is unusable).
/// Per-connection negotiation errors are logged and never propagate.
pub async fn serve_broker_session_endpoint<R>(
    listener: interprocess::local_socket::tokio::Listener,
    responder: &R,
    daemon_session_endpoint: &str,
) -> std::io::Result<()>
where
    R: HelloResponder + ?Sized,
{
    use interprocess::local_socket::tokio::prelude::*;

    loop {
        let stream = listener.accept().await?;
        // TODO(soldr#2365): real peer creds on tokio-interprocess streams.
        // Until then this spike does not enforce PeerCredentialPolicy.
        let peer = placeholder_peer();
        match negotiate_session_hello(stream, responder, peer).await {
            Ok(Some(stream)) => {
                let endpoint = daemon_session_endpoint.to_owned();
                tokio::spawn(async move {
                    if let Err(err) = relay_session(stream, &endpoint).await {
                        eprintln!("running-process-broker: session relay ended: {err}");
                    }
                });
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!("running-process-broker: session hello failed: {err}");
            }
        }
    }
}

/// Run one async Hello round-trip over `stream` and, if it negotiated a
/// SESSION, return the same `stream` for the caller to relay; otherwise
/// return `None`.
///
/// This is the async analog of `handle_control_connection` for the Hello step:
/// it reads the framed Hello, calls the sync `responder`, and writes the framed
/// reply. The stream is returned by value (never split or buffered) so the
/// caller can move it into a relay task with its byte stream untouched — the
/// client's first `SessionStart` bytes are still unread on the wire.
///
/// # Errors
///
/// Transport errors reading or writing the Hello frame. A malformed or
/// oversized Hello is answered with a `Refused` reply and returns `Ok(None)`
/// (not an error) — the peer was served, just not negotiated.
pub async fn negotiate_session_hello<S, R>(
    mut stream: S,
    responder: &R,
    peer: PeerIdentity,
) -> std::io::Result<Option<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
    R: HelloResponder + ?Sized,
{
    let request_bytes = match read_hello_frame(&mut stream).await? {
        Ok(bytes) => bytes,
        Err(reply) => {
            write_hello_response(&mut stream, None, &reply).await?;
            return Ok(None);
        }
    };

    let request_frame = match Frame::decode(request_bytes.as_slice()) {
        Ok(frame) => frame,
        Err(_) => {
            let reply = refused_reply(ErrorCode::ErrorPeerRejected, "malformed broker Frame", 0);
            write_hello_response(&mut stream, None, &reply).await?;
            return Ok(None);
        }
    };

    let reply = responder.handle_frame(request_frame.clone(), peer);
    write_hello_response(&mut stream, Some(&request_frame), &reply).await?;

    Ok(negotiated(&reply).map(|_| stream))
}

/// Return the `Negotiated` payload when `reply` negotiated a backend.
///
/// A successful negotiation is the spike's "proceed to SESSION" signal. The
/// SESSION-vs-handoff distinction (and reading the daemon endpoint from
/// `Negotiated::backend_pipe` instead of a caller-supplied path) is a
/// follow-up once this transport is proven.
fn negotiated(reply: &HelloReply) -> Option<&Negotiated> {
    match reply.result.as_ref()? {
        HelloReplyResult::Negotiated(n) => Some(n),
        _ => None,
    }
}

/// Read exactly one `[1][u32 LE len][body]` Hello frame off `stream`.
///
/// Manual `read_exact` of precisely `5 + len` bytes — **never** a buffering
/// codec — so nothing past the Hello frame is consumed. `Ok(Err(reply))` is a
/// well-formed refusal to write back (bad version / oversize / short read);
/// `Err(..)` is a fatal transport error.
async fn read_hello_frame<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> std::io::Result<Result<Vec<u8>, HelloReply>> {
    let mut version = [0u8; 1];
    if read_exact_eof(stream, &mut version).await? {
        return Ok(Err(refused_reply(
            ErrorCode::ErrorPeerRejected,
            "incomplete Hello frame",
            0,
        )));
    }
    if version[0] != ENVELOPE_VERSION {
        return Ok(Err(refused_reply(
            ErrorCode::ErrorVersionUnsupported,
            "unsupported framing version",
            0,
        )));
    }

    let mut len_buf = [0u8; 4];
    if read_exact_eof(stream, &mut len_buf).await? {
        return Ok(Err(refused_reply(
            ErrorCode::ErrorPeerRejected,
            "incomplete Hello frame",
            0,
        )));
    }
    let body_len = u32::from_le_bytes(len_buf) as usize;
    if body_len > MAX_HELLO_BYTES {
        return Ok(Err(refused_reply(
            ErrorCode::ErrorPeerRejected,
            "initial Hello frame exceeds 64 KiB",
            0,
        )));
    }

    let mut body = vec![0u8; body_len];
    if body_len > 0 && read_exact_eof(stream, &mut body).await? {
        return Ok(Err(refused_reply(
            ErrorCode::ErrorPeerRejected,
            "incomplete Hello frame",
            0,
        )));
    }
    Ok(Ok(body))
}

/// `read_exact` that maps a clean EOF to `Ok(true)` (peer hung up) instead of
/// an error, and any other I/O failure to `Err`.
async fn read_exact_eof<S: AsyncRead + Unpin>(
    stream: &mut S,
    buf: &mut [u8],
) -> std::io::Result<bool> {
    match stream.read_exact(buf).await {
        Ok(_) => Ok(false),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(true),
        Err(err) => Err(err),
    }
}

/// Frame a [`HelloReply`] as a `Response` and write it as `[1][u32 LE len][Frame]`.
///
/// Byte-for-byte the same response frame the sync `write_response_frame` emits;
/// only the write is async.
async fn write_hello_response<S: AsyncWrite + Unpin>(
    stream: &mut S,
    request_frame: Option<&Frame>,
    reply: &HelloReply,
) -> std::io::Result<()> {
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
    let body = response_frame.encode_to_vec();
    let len = u32::try_from(body.len())
        .map_err(|_| std::io::Error::other("broker response frame exceeds u32 length"))?;
    let mut header = [0u8; 5];
    header[0] = ENVELOPE_VERSION;
    header[1..].copy_from_slice(&len.to_le_bytes());
    stream.write_all(&header).await?;
    if !body.is_empty() {
        stream.write_all(&body).await?;
    }
    stream.flush().await?;
    Ok(())
}

/// Placeholder peer identity used until async peer creds are resolved.
///
/// See the module-level "Not yet wired to the binary" note: this is why the
/// spike must not be reachable from `running-process-broker-v2` yet.
fn placeholder_peer() -> PeerIdentity {
    PeerIdentity {
        pid: std::process::id(),
        uid_or_sid: String::new(),
    }
}

#[cfg(all(test, feature = "daemon"))]
mod tests;
