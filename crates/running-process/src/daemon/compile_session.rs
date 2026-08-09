//! Phase 3 daemon-side compile-session handler (soldr#2365, slice 3c).
//!
//! Models the daemon's existing connection-takeover streaming handlers
//! ([`crate::daemon::pipe_attach_stream`]): once a session starts, the whole
//! transport is handed here for the session's lifetime. This handler runs a
//! **byte-transparent** compile session — the proxy pump ([`run_child_session`])
//! driving a **contained** child ([`spawn_contained_session`], a rustc-class
//! process in its own Job Object / process group) — and bridges the pump's
//! synchronous `SessionFrame` channels to the async transport.
//!
//! **Framing (Model B, soldr#2365).** The SESSION plane rides the broker's
//! backend-mux wire: each `SessionFrame` is wrapped in a `Frame` on the
//! `SESSION_PAYLOAD_PROTOCOL` (`0x5350`) lane and framed as
//! `[1][u32 len][prost Frame]` — exactly what [`BackendEndpointMux`] classifies
//! and what the merged SESSION codec (soldr#2365 slice 3a,
//! [`crate::broker::session_codec`]) produces. [`SessionFrameCodec`] is the thin
//! tokio codec over that sans-io wire, so this handler speaks the mux/Model-B
//! framing rather than the daemon's legacy `LengthDelimitedCodec`/`DaemonRequest`
//! surface. The next slice registers this lane on a real
//! [`BackendEndpointMux`] backend in the daemon's accept loop.
//!
//! [`BackendEndpointMux`]: crate::broker::backend_sdk::BackendEndpointMux
//!
//! **Backpressure / bounded memory (soldr#2365 invariant).** The outbound path
//! is a **bounded** async channel; the pump feeds it via a blocking send
//! ([`FrameSink`] for the tokio sender), so a slow client stalls the pump's
//! reader thread, fills the child's OS pipe, and backpressures the child — output
//! is never dropped (byte-exact) and per-session memory stays bounded.
//!
//! Not yet wired into the accept loop's mux dispatch; this slice proves the
//! handler + bridge over the Model-B wire via a tokio-`duplex()` daemon-direct
//! test, run in CI by a scoped `--features daemon` nextest step.

use std::process::Command;
use std::sync::Arc;

use bytes::BytesMut;
use futures_util::{SinkExt, StreamExt};
use tokio_util::codec::{Decoder, Encoder, Framed};

use crate::broker::protocol_v2::{session_frame, SessionExit, SessionFrame};
use crate::broker::session_codec::{encode_session_frame, try_decode_session_frame};
use crate::broker::session_pump::{run_child_session, FrameSink};
use crate::broker::session_server::spawn_contained_session;
use crate::containment::ContainedProcessGroup;

/// Bounded async sink for the pump's outbound frames. `blocking_send` stalls the
/// pump's (blocking-thread / std-thread) reader when the channel is full, which
/// backpressures the child. Safe because the pump never runs on an async worker
/// thread — it is driven under `spawn_blocking` and its own `std::thread`s. On
/// failure the un-sent frame is returned, matching the trait contract.
impl FrameSink for tokio::sync::mpsc::Sender<SessionFrame> {
    fn send(&self, frame: SessionFrame) -> Result<(), SessionFrame> {
        self.blocking_send(frame).map_err(|e| e.0)
    }
}

/// Tokio codec framing `SessionFrame`s on the SESSION lane using the broker's
/// `[1][u32 len][Frame{payload_protocol=0x5350}]` wire — the same framing
/// [`BackendEndpointMux`](crate::broker::backend_sdk::BackendEndpointMux)
/// classifies. Wraps the sans-io [`encode_session_frame`] /
/// [`try_decode_session_frame`] so the async handler can speak the mux wire with
/// ordinary `Framed` ergonomics.
#[derive(Default)]
pub struct SessionFrameCodec {
    /// Session-local outbound sequence, carried in each `Frame`'s `request_id`
    /// for observability (see the codec module docs).
    seq: u64,
}

impl Decoder for SessionFrameCodec {
    type Item = SessionFrame;
    type Error = std::io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<SessionFrame>, std::io::Error> {
        match try_decode_session_frame(buf) {
            Ok(Some(decoded)) => {
                let _ = buf.split_to(decoded.consumed);
                Ok(Some(decoded.frame))
            }
            Ok(None) => Ok(None),
            Err(err) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, err)),
        }
    }
}

impl Encoder<SessionFrame> for SessionFrameCodec {
    type Error = std::io::Error;

    fn encode(&mut self, frame: SessionFrame, buf: &mut BytesMut) -> Result<(), std::io::Error> {
        let wire = encode_session_frame(&frame, self.seq)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        self.seq = self.seq.wrapping_add(1);
        buf.extend_from_slice(&wire);
        Ok(())
    }
}

/// Wrap a raw async transport as a SESSION-lane framed stream (the Model-B wire).
pub fn session_framed<T>(io: T) -> Framed<T, SessionFrameCodec>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite,
{
    Framed::new(io, SessionFrameCodec::default())
}

/// Outbound backpressure-channel depth, in frames. Bounds per-session memory:
/// at most this many pump-produced frames (each ≤ the pump's 8 KiB read chunk)
/// buffer ahead of a slow client before the child is stalled.
const OUTBOUND_FRAME_CAPACITY: usize = 64;

/// Run a compile session over `framed`: spawn `command` as a contained child,
/// proxy its stdio byte-for-byte as SESSION-lane `SessionFrame`s, apply inbound
/// stdin frames, and return the child's [`SessionExit`].
///
/// The peer speaks SESSION-lane frames (`Stdin` / `StdinEof` inbound, `Stdout` /
/// `Stderr` / `Exit` outbound). The handler returns once the child exits
/// (terminal `Exit` sent) or the peer disconnects.
///
/// # Errors
///
/// Propagates a spawn failure, a transport write/read error, or a failure to reap
/// the child. Never errors on stdio content.
pub async fn run_compile_session<T>(
    mut framed: Framed<T, SessionFrameCodec>,
    mut command: Command,
    group: Arc<ContainedProcessGroup>,
) -> std::io::Result<SessionExit>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let child = spawn_contained_session(&group, &mut command)?;

    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<SessionFrame>(OUTBOUND_FRAME_CAPACITY);
    let (stdin_tx, stdin_rx) = std::sync::mpsc::channel::<SessionFrame>();

    // Drive the byte-transparent pump on a blocking thread: it feeds the bounded
    // `out_tx` (backpressure) and consumes decoded stdin frames from `stdin_rx`.
    let pump = tokio::task::spawn_blocking(move || run_child_session(child, out_tx, stdin_rx));

    // Hold the stdin sender in an Option so we can drop it (signalling stdin EOF
    // to the pump) the moment the peer disconnects, before awaiting the pump.
    let mut stdin_tx = Some(stdin_tx);
    loop {
        tokio::select! {
            outbound = out_rx.recv() => {
                match outbound {
                    Some(frame) => {
                        let terminal = matches!(frame.kind, Some(session_frame::Kind::Exit(_)));
                        framed
                            .send(frame)
                            .await
                            .map_err(|e| std::io::Error::other(format!("session send failed: {e}")))?;
                        if terminal {
                            break;
                        }
                    }
                    // Pump finished without a terminal frame (should not happen —
                    // the pump always emits Exit); stop draining.
                    None => break,
                }
            }
            inbound = framed.next() => {
                match inbound {
                    Some(Ok(frame)) => {
                        // `StdinEof` is the peer's "no more stdin" signal. We must
                        // DROP `stdin_tx` after forwarding it: the pump joins its
                        // stdin thread (which drains until the sender drops) *before*
                        // emitting the terminal `Exit`, so holding `stdin_tx` past EOF
                        // deadlocks — the pump waits to send `Exit` while we wait to
                        // receive it. (The sync `serve_session` avoids this because
                        // its client closes the transport, EOF-ing the reader; a
                        // takeover connection stays open for output, so the signal
                        // must be the frame, not a socket close.)
                        let is_eof =
                            matches!(frame.kind, Some(session_frame::Kind::StdinEof(_)));
                        if let Some(tx) = stdin_tx.as_ref() {
                            // Pump gone (child already exited): stop forwarding.
                            if tx.send(frame).is_err() {
                                stdin_tx = None;
                            }
                        }
                        if is_eof {
                            stdin_tx = None;
                        }
                    }
                    Some(Err(e)) => {
                        return Err(std::io::Error::other(format!("session recv failed: {e}")))
                    }
                    // Peer disconnected: close stdin so a child blocked on read makes
                    // progress, then let the child run to completion.
                    None => {
                        stdin_tx = None;
                    }
                }
            }
        }
    }

    // Ensure the pump's stdin side is closed, then reap it.
    drop(stdin_tx);
    match pump.await {
        Ok(result) => result,
        Err(join_err) => Err(std::io::Error::other(format!(
            "compile-session pump panicked: {join_err}"
        ))),
    }
}

#[cfg(test)]
mod tests;
