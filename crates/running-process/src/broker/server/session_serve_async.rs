//! Async v2 broker SESSION serve path — the strangler-fig async twin of the
//! synchronous control-socket loop (soldr#2365).
//!
//! The owner-directed async reversal: the v2 broker's SESSION data plane is
//! full-proxy (client → broker → daemon, every byte relayed), which needs the
//! platform relay and a tokio-`interprocess` accept loop.
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
//! 2. route it through the sync `responder` to a `HelloReply`;
//! 3. write the framed reply back;
//! 4. on a negotiated SESSION, hand the **same** connection to
//!    [`relay_local_socket_session`](crate::broker::session_relay::relay_local_socket_session), which
//!    dials the daemon SESSION endpoint and proxies bytes both ways.
//!
//! # Peer-credential enforcement
//!
//! The accept loop reads OS peer credentials off each tokio-`interprocess`
//! stream via `peer_identity_from_tokio_stream` (`peer_creds()` is a
//! synchronous, non-blocking `getsockopt` query — safe from async) and refuses
//! any peer the [`PeerCredentialPolicy`] rejects **before** reading a byte of
//! Hello, exactly as the sync loop does. This is the parity that lets the path
//! be wired into `running-process-broker-v2`.

use prost::Message;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::broker::protocol::{
    hello_reply::Result as HelloReplyResult, ErrorCode, Frame, FrameKind, HelloReply, Negotiated,
    PayloadEncoding, CONTROL_PAYLOAD_PROTOCOL, ENVELOPE_VERSION, MAX_HELLO_BYTES, PROTOCOL_VERSION,
};
use crate::broker::session_relay::relay_local_socket_session;

use super::connection::{
    local_socket_name, peer_identity_from_tokio_stream, refused_reply, HelloResponder,
    PeerCredentialPolicy,
};
use super::hello_handler::PeerIdentity;

// soldr#2451: this bounds concurrent Hello NEGOTIATIONS only (the relay no
// longer holds a permit). Negotiations are a sub-millisecond round-trip, so a
// generous cap keeps the accept loop from stalling when cargo opens a burst of
// per-compile connections at once, while still guarding the blocking pool
// against an unbounded task fan-out. 8 was sized as if it capped live compiles;
// it does not, so it can be much higher.
const MAX_CONCURRENT_SESSION_NEGOTIATIONS: usize = 256;

/// Bind `socket_path` as a tokio SESSION listener and serve it via
/// [`serve_broker_session_endpoint`].
///
/// This is the production entry point: real callers (e.g. `soldr broker serve`)
/// hold a socket-path string and a [`HelloResponder`] (typically a
/// `HelloRouter`) — not a pre-bound `interprocess::local_socket::tokio::Listener`.
/// It resolves the platform local-socket name the same way the sync broker bind
/// does ([`local_socket_name`]) and creates the tokio listener.
///
/// # Errors
///
/// Fails if the socket path cannot be bound (e.g. another broker already owns
/// it), or with the first fatal `accept()` error from the serve loop.
pub async fn serve_broker_session_socket<R>(
    socket_path: &str,
    responder: &R,
    peer_policy: &PeerCredentialPolicy,
) -> std::io::Result<()>
where
    R: HelloResponder + ?Sized,
{
    let listener = bind_session_listener(socket_path)?;
    serve_broker_session_endpoint(listener, responder, peer_policy).await
}

/// Bind a tokio-`interprocess` SESSION listener at `socket_path`.
///
/// Uses the same platform name resolution ([`local_socket_name`]) as the sync
/// broker bind, so a SESSION listener and a legacy control listener name the
/// same path identically across Unix (filesystem) and Windows (namespace).
fn bind_session_listener(
    socket_path: &str,
) -> std::io::Result<interprocess::local_socket::tokio::Listener> {
    use interprocess::local_socket::ListenerOptions;

    let name = local_socket_name(socket_path)?;
    ListenerOptions::new().name(name).create_tokio()
}

/// Accept SESSION connections on `listener`, negotiate each Hello with the sync
/// `responder`, and full-proxy every negotiated session to the daemon SESSION
/// endpoint the negotiation itself selected (`Negotiated.backend_pipe`).
///
/// The relay target is resolved **per connection** from the reply, exactly as
/// the broker binary resolves it per Hello (`resolve_backend_pipe`): in the
/// full-proxy model `backend_pipe` stops being "where the client reconnects"
/// and becomes "where the broker relays". A negotiation that yields an empty
/// `backend_pipe` (no daemon has published for the service yet) is dropped
/// rather than relayed to nowhere.
///
/// Each peer is refused up front unless `peer_policy` allows its OS
/// credentials — the same foreign-peer rejection the sync loop performs before
/// reading any Hello bytes. The Hello round-trip then runs inline (it borrows
/// `responder`, which holds broker-owned non-`Send` routing state, exactly as
/// the sync loop does), so negotiation is sequential and cheap; the byte relay
/// — the only long-lived part — is spawned per connection so concurrent
/// compiles never serialize.
///
/// # Errors
///
/// Returns the first fatal `accept()` error (the listener is unusable).
/// Per-connection credential/negotiation errors are logged and never propagate.
pub async fn serve_broker_session_endpoint<R>(
    listener: interprocess::local_socket::tokio::Listener,
    responder: &R,
    peer_policy: &PeerCredentialPolicy,
) -> std::io::Result<()>
where
    R: HelloResponder + ?Sized,
{
    use interprocess::local_socket::tokio::prelude::*;

    loop {
        let stream = listener.accept().await?;
        let peer = match peer_identity_from_tokio_stream(&stream) {
            Ok(peer) => peer,
            Err(err) => {
                eprintln!("running-process-broker: could not read peer credentials: {err}");
                continue;
            }
        };
        if !peer_policy.allows(&peer) {
            eprintln!(
                "running-process-broker: dropped session peer pid={} uid_or_sid={:?}: \
                 credential policy refused",
                peer.pid, peer.uid_or_sid
            );
            continue;
        }
        match negotiate_session_hello(stream, responder, peer).await {
            Ok(Some((stream, backend_pipe))) => {
                if backend_pipe.is_empty() {
                    eprintln!(
                        "running-process-broker: negotiated a session but no backend endpoint \
                         is published; dropping"
                    );
                    continue;
                }
                tokio::spawn(async move {
                    if let Err(err) = relay_local_socket_session(stream, &backend_pipe).await {
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

/// Accept SESSION connections with bounded concurrent Hello routing.
///
/// Unlike [`serve_broker_session_endpoint`], this entry point supports a
/// responder whose synchronous routing step may perform blocking launch and
/// readiness work. Accepted peers are bounded by a semaphore, frame I/O stays
/// async, and only `HelloResponder::handle_frame` runs on Tokio's blocking
/// pool. This keeps the listener responsive without creating an unbounded
/// collection of tasks or threads.
pub async fn serve_broker_session_endpoint_concurrently<R>(
    listener: interprocess::local_socket::tokio::Listener,
    responder: Arc<R>,
    peer_policy: &PeerCredentialPolicy,
) -> std::io::Result<()>
where
    R: HelloResponder + Send + Sync + 'static,
{
    use interprocess::local_socket::tokio::prelude::*;

    let permits = Arc::new(tokio::sync::Semaphore::new(
        MAX_CONCURRENT_SESSION_NEGOTIATIONS,
    ));
    loop {
        let stream = listener.accept().await?;
        let peer = match peer_identity_from_tokio_stream(&stream) {
            Ok(peer) => peer,
            Err(err) => {
                eprintln!("running-process-broker: could not read peer credentials: {err}");
                continue;
            }
        };
        if !peer_policy.allows(&peer) {
            eprintln!(
                "running-process-broker: dropped session peer pid={} uid_or_sid={:?}: \
                 credential policy refused",
                peer.pid, peer.uid_or_sid
            );
            continue;
        }
        let permit = Arc::clone(&permits)
            .acquire_owned()
            .await
            .map_err(|_| std::io::Error::other("SESSION negotiation pool closed"))?;
        let responder = Arc::clone(&responder);
        tokio::spawn(async move {
            // soldr#2451: the permit must bound ONLY the negotiation (a fast
            // Hello round-trip whose `handle_frame` runs on the blocking pool),
            // NOT the relay below. `relay_session` is pure async
            // splice/buffered proxy that lives for the whole compile (seconds);
            // holding the permit across it turned this negotiation-concurrency
            // bound into a hard cap on *concurrent compiles*. With cargo's
            // pipelining running far more than the cap's worth of rustc at once,
            // the accept loop blocked on `acquire_owned`, dials overflowed the
            // OS backlog, and ~4% were refused — surfacing on cross-builds as
            // "SESSION relay closed before Exit" / broker-unreachable hard
            // fails. Scope the permit to negotiation; relays scale freely.
            let negotiated = {
                let _permit = permit;
                negotiate_session_hello_concurrently(stream, responder, peer).await
            };
            match negotiated {
                Ok(Some((stream, backend_pipe))) => {
                    if backend_pipe.is_empty() {
                        eprintln!(
                            "running-process-broker: negotiated a session but no backend endpoint \
                             is published; dropping"
                        );
                        return;
                    }
                    if let Err(err) = relay_local_socket_session(stream, &backend_pipe).await {
                        eprintln!("running-process-broker: session relay ended: {err}");
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    eprintln!("running-process-broker: session hello failed: {err}");
                }
            }
        });
    }
}

async fn negotiate_session_hello_concurrently<S, R>(
    mut stream: S,
    responder: Arc<R>,
    peer: PeerIdentity,
) -> std::io::Result<Option<(S, String)>>
where
    S: AsyncRead + AsyncWrite + Unpin,
    R: HelloResponder + Send + Sync + 'static,
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
    let route_frame = request_frame.clone();
    let reply = tokio::task::spawn_blocking(move || responder.handle_frame(route_frame, peer))
        .await
        .map_err(|err| std::io::Error::other(format!("SESSION route worker failed: {err}")))?;
    write_hello_response(&mut stream, Some(&request_frame), &reply).await?;
    let backend_pipe = negotiated(&reply).map(|n| n.backend_pipe.clone());
    Ok(backend_pipe.map(|pipe| (stream, pipe)))
}

/// Run one async Hello round-trip over `stream` and, if it negotiated a
/// SESSION, return the same `stream` paired with the daemon SESSION endpoint
/// the reply selected (`Negotiated.backend_pipe`); otherwise return `None`.
///
/// This is the async analog of `handle_control_connection` for the Hello step:
/// it reads the framed Hello, calls the sync `responder`, and writes the framed
/// reply. The stream is returned by value (never split or buffered) so the
/// caller can move it into a relay task with its byte stream untouched — the
/// client's first `SessionStart` bytes are still unread on the wire. The
/// returned `backend_pipe` may be empty when negotiation succeeded but no
/// daemon has published for the service; the caller decides how to handle that.
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
) -> std::io::Result<Option<(S, String)>>
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

    let backend_pipe = negotiated(&reply).map(|n| n.backend_pipe.clone());
    Ok(backend_pipe.map(|pipe| (stream, pipe)))
}

/// Return the `Negotiated` payload when `reply` negotiated a backend.
///
/// A successful negotiation is the "proceed to SESSION" signal; its
/// `backend_pipe` is the daemon SESSION endpoint the broker relays to. The
/// SESSION-vs-adopt distinction on a shared socket (if the design mounts one
/// socket for both) is a follow-up decided by the #2360/#2365 socket model.
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

/// Frame a `HelloReply` as a `Response` and write it as `[1][u32 LE len][Frame]`.
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

#[cfg(all(test, feature = "daemon"))]
mod tests;
