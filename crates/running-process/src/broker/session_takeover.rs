//! Async SESSION-lane takeover handler (soldr#2365): running-process's
//! **generic spawn-and-stream** SESSION execution, on the `client-async`
//! feature so any async consumer daemon can serve a SESSION by spawning the
//! command the client sent and proxying its stdio.
//!
//! # Not soldr's compile path (Fable 5 ruling on #2365 / #2387)
//!
//! **soldr does NOT use this handler for compiles.** soldr executes compiles
//! in-process through its embedded zccache service, not by spawning a child, so
//! its SESSION seam is [`crate::broker::session_codec`] alone: it decodes the
//! opening `SessionStart` argv, runs the compile through zccache, and encodes
//! the captured output back as `SessionFrame`s. It never calls
//! [`session_takeover_from_buffered`] / [`serve_session`] / [`run_child_session`].
//! This handler is for running-process's own generic consumers (spawn a
//! command, stream its stdio); do not route soldr compiles through it.
//!
//! This is the byte-transparent handler (the proxy pump [`run_child_session`]
//! driving a **contained** child, [`spawn_contained_session`]) bridged onto the
//! async transport. It lived under the `daemon` feature as
//! `daemon::compile_session`; it moved here so `client-async` consumers can
//! reach it (`daemon::compile_session` re-exports it unchanged).
//!
//! **Framing (Model B, soldr#2365).** Each `SessionFrame` rides one `Frame` on
//! the `SESSION_PAYLOAD_PROTOCOL` (`0x5350`) lane, framed `[1][u32 len][prost
//! Frame]` — exactly what [`BackendEndpointMux`] classifies and what the broker
//! relay carries transparently. [`SessionFrameCodec`] is the thin tokio codec
//! over the sans-io [`encode_session_frame`] / [`try_decode_session_frame`] wire.
//!
//! [`BackendEndpointMux`]: crate::broker::backend_sdk::BackendEndpointMux
//!
//! **Backpressure / bounded memory.** The outbound path is a **bounded** async
//! channel; the pump feeds it via a blocking send ([`FrameSink`] for the tokio
//! sender), so a slow peer stalls the pump's reader thread, fills the child's OS
//! pipe, and backpressures the child — output is never dropped (byte-exact) and
//! per-session memory stays bounded.

use std::process::Command;
use std::sync::Arc;

use bytes::BytesMut;
use futures_util::{SinkExt, StreamExt};
use tokio_util::codec::{Decoder, Encoder, Framed, FramedParts};

use crate::broker::protocol_v2::{session_frame, SessionExit, SessionFrame, SessionStart};
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

/// Build the compiler [`Command`] from a session's opening [`SessionStart`].
///
/// Mirrors the daemon's `SpawnPipeSession` env semantics: `clear_inherited_env`
/// wipes the inherited environment first; `env` entries are then layered in
/// order (later entries win case collisions the way `Command::env` does).
fn command_from_start(start: &SessionStart) -> Command {
    // allow-raw-command-new: the command is spawned only through the sanitized
    // `spawn_contained_session` layer below; this just describes it.
    let mut command = Command::new(&start.program);
    command.args(&start.args);
    if !start.cwd.is_empty() {
        command.current_dir(&start.cwd);
    }
    if start.clear_inherited_env {
        command.env_clear();
    }
    for entry in &start.env {
        command.env(&entry.key, &entry.value);
    }
    command
}

/// Take over `io` — an async transport whose read side may already hold
/// `prebuffered` bytes — as a SESSION for its lifetime.
///
/// This is the entry a consumer daemon's mux accept loop calls once it has
/// classified a `0x5350` frame: it hands the connection (with the already-read
/// bytes, so the opening `SessionStart` is not lost) to [`serve_session`]. The
/// `prebuffered` bytes are seeded into the codec's read buffer via
/// [`FramedParts`], never re-parsed by the caller.
///
/// # Errors
///
/// Propagates any error from [`serve_session`] (spawn failure, transport error,
/// or a failure to reap the child).
pub async fn session_takeover_from_buffered<T>(
    io: T,
    prebuffered: BytesMut,
    group: Arc<ContainedProcessGroup>,
) -> std::io::Result<SessionExit>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut parts = FramedParts::new(io, SessionFrameCodec::default());
    parts.read_buf = prebuffered;
    serve_session(Framed::from_parts(parts), group).await
}

/// Serve a compile session whose command is carried **on the wire**: read the
/// mandatory opening [`SessionStart`] frame, build the contained child from it,
/// and proxy the rest of the session via [`run_compile_session`].
///
/// This is the real serve entry — the command comes from the client, not a
/// caller-passed [`Command`]. A session that does not open with exactly one
/// `SessionStart` is a protocol error.
///
/// # Errors
///
/// Errors if the peer hangs up before sending `SessionStart`, sends a different
/// first frame, or on any failure propagated by [`run_compile_session`].
pub async fn serve_session<T>(
    mut framed: Framed<T, SessionFrameCodec>,
    group: Arc<ContainedProcessGroup>,
) -> std::io::Result<SessionExit>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let first = framed
        .next()
        .await
        .ok_or_else(|| std::io::Error::other("session closed before SessionStart"))?
        .map_err(|e| std::io::Error::other(format!("session recv failed before start: {e}")))?;
    let start = match first.kind {
        Some(session_frame::Kind::Start(start)) => start,
        other => {
            return Err(std::io::Error::other(format!(
                "session must open with SessionStart, got {other:?}"
            )))
        }
    };
    let command = command_from_start(&start);
    run_compile_session(framed, command, group).await
}

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
