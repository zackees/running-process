//! Phase 3 daemon-side compile-session handler (soldr#2365, slice 3c).
//!
//! Models the daemon's existing connection-takeover streaming handlers
//! ([`crate::daemon::pipe_attach_stream`]): once a session starts, the whole
//! [`Framed`] transport is handed here for the session's lifetime. This handler
//! runs a **byte-transparent** compile session — the proxy pump
//! ([`run_child_session`]) driving a **contained** child ([`spawn_contained_session`],
//! a rustc-class process in its own Job Object / process group) — and bridges the
//! pump's synchronous `SessionFrame` channels to the async transport.
//!
//! **Framing.** The daemon transport is `Framed<_, LengthDelimitedCodec>`, which
//! already length-delimits each body, so this handler sends **one prost
//! `SessionFrame` per body** (not the broker `encode_framed` `[1][len][Frame]`
//! envelope — that is the broker-relay lane's concern, a later slice). Inbound
//! bodies decode straight back to `SessionFrame`.
//!
//! **Backpressure / bounded memory (soldr#2365 invariant).** The outbound path
//! is a **bounded** async channel; the pump feeds it via a blocking send
//! ([`FrameSink`] for the tokio sender), so a slow client stalls the pump's
//! reader thread, fills the child's OS pipe, and backpressures the child — output
//! is never dropped (byte-exact) and per-session memory stays bounded.
//!
//! Not yet wired into `handle_connection`'s dispatch (no `RequestType` branch
//! yet); this slice proves the handler + bridge over the real transport type via
//! a tokio-`duplex()` daemon-direct test, run in CI by a scoped `--features
//! daemon` nextest step.

use std::process::Command;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::broker::protocol_v2::{session_frame, SessionExit, SessionFrame};
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

/// Outbound backpressure-channel depth, in frames. Bounds per-session memory:
/// at most this many pump-produced frames (each ≤ the pump's 8 KiB read chunk)
/// buffer ahead of a slow client before the child is stalled.
const OUTBOUND_FRAME_CAPACITY: usize = 64;

/// Run a compile session over `framed`: spawn `command` as a contained child,
/// proxy its stdio byte-for-byte as `SessionFrame`s, apply inbound stdin frames,
/// and return the child's [`SessionExit`].
///
/// The client speaks length-delimited `SessionFrame` prost messages: `Stdin` /
/// `StdinEof` inbound, `Stdout` / `Stderr` / `Exit` outbound. The handler returns
/// once the child exits (terminal `Exit` sent) or the client disconnects.
///
/// # Errors
///
/// Propagates a spawn failure, a transport write/read error, or a failure to reap
/// the child. Never errors on stdio content.
pub async fn run_compile_session<T>(
    mut framed: Framed<T, LengthDelimitedCodec>,
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
    // to the pump) the moment the client disconnects, before awaiting the pump.
    let mut stdin_tx = Some(stdin_tx);
    loop {
        tokio::select! {
            outbound = out_rx.recv() => {
                match outbound {
                    Some(frame) => {
                        let terminal = matches!(frame.kind, Some(session_frame::Kind::Exit(_)));
                        framed
                            .send(Bytes::from(frame.encode_to_vec()))
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
                    // A malformed inbound body is ignored (the `if let` drops the
                    // decode error) rather than tearing the session down.
                    Some(Ok(bytes)) => {
                        if let Ok(frame) = SessionFrame::decode(bytes.as_ref()) {
                            // `StdinEof` is the client's "no more stdin" signal.
                            // We must DROP `stdin_tx` after forwarding it: the pump
                            // joins its stdin thread (which drains until the sender
                            // drops) *before* emitting the terminal `Exit`, so
                            // holding `stdin_tx` past EOF deadlocks — the pump waits
                            // to send `Exit` while we wait to receive it. (The sync
                            // `serve_session` avoids this because its client closes
                            // the transport, EOF-ing the reader; the daemon
                            // connection stays open for output, so the signal must
                            // be the frame, not a socket close.)
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
                    }
                    Some(Err(e)) => {
                        return Err(std::io::Error::other(format!("session recv failed: {e}")))
                    }
                    // Client disconnected: close stdin so a child blocked on read
                    // makes progress, then let the child run to completion.
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
