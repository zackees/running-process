//! Phase 3 session server (soldr#2365, slice 3b): run a **contained** child as a
//! broker-proxied session over a byte transport.
//!
//! This ties together three already-merged pieces:
//!   - the byte-transparent proxy pump ([`crate::broker::session_pump`]),
//!   - the SESSION-lane codec ([`crate::broker::session_codec`]),
//!   - the sanitized contained-spawn layer
//!     ([`ContainedProcessGroup`] → [`SpawnedChild`], a child confined to its own
//!     Job Object on Windows / process group on Unix, killed when dropped).
//!
//! [`serve_session`] reads inbound `SessionFrame`s off a reader `R`, applies them
//! to the child's stdin, streams the child's stdout/stderr/exit back out as
//! `SessionFrame`s on a writer `W`, and reaps the child. It is generic over the
//! two transport halves (`R: Read` inbound, `W: Write` outbound), matching
//! [`crate::broker::backend_sdk::FrameClient::from_stream`]'s
//! generic-over-stream grain, so the real broker `local_socket` — whose
//! raw-duplex takeover (`into_backend_io`) is Windows-deferred (#720) — is wired
//! in a later slice without changing this code.
//!
//! Nothing dials this yet; it is additive and dormant.
//!
//! **Client contract:** the client closes its inbound-writing half once it has
//! sent stdin + `StdinEof`. `serve_session` returns only after the child exits,
//! the inbound stream reaches EOF, and all outbound frames are flushed — so a
//! client that holds the inbound half open forever keeps the session's stdin
//! pump thread alive. This mirrors the pump's existing `stdin_rx`-closed
//! contract and is exactly what a dumb-terminal client (slice 4) does.

use std::io::{self, Read, Write};
use std::process::Command;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use crate::broker::protocol_v2::{SessionExit, SessionFrame};
use crate::broker::session_codec::{encode_session_frame, try_decode_session_frame};
use crate::broker::session_pump::{run_child_session, SessionChild};
use crate::containment::ContainedProcessGroup;
use crate::spawn::{SpawnStdio, SpawnedChild, StdioSource};

impl SessionChild for SpawnedChild {
    type Stdin = std::process::ChildStdin;
    type Stdout = std::process::ChildStdout;
    type Stderr = std::process::ChildStderr;

    fn take_stdin(&mut self) -> Option<Self::Stdin> {
        self.stdin.take()
    }
    fn take_stdout(&mut self) -> Option<Self::Stdout> {
        self.stdout.take()
    }
    fn take_stderr(&mut self) -> Option<Self::Stderr> {
        self.stderr.take()
    }
    fn wait_session(&mut self) -> io::Result<SessionExit> {
        // The sanitized contained-spawn layer reports a single exit code (its
        // own `unix_exit_code` mapping folds a signal death into that code), so
        // `signal` is always 0 on this path. The daemon keys on the code.
        Ok(SessionExit {
            code: self.wait()?,
            signal: 0,
        })
    }
}

/// Spawn `command` as a contained child — its own Job Object (Windows) /
/// process group (Unix), killed when the returned [`SpawnedChild`] drops — with
/// all three stdio streams piped, ready to hand to [`serve_session`] or
/// [`run_child_session`].
///
/// The pipes carry raw bytes (no line splitting), which is what preserves the
/// byte-for-byte fidelity the pump guarantees.
///
/// # Errors
///
/// Propagates any spawn failure from the sanitized contained-spawn layer.
pub fn spawn_contained_session(
    group: &ContainedProcessGroup,
    command: &mut Command,
) -> io::Result<SpawnedChild> {
    let stdio = SpawnStdio {
        stdin: StdioSource::Pipe,
        stdout: StdioSource::Pipe,
        stderr: StdioSource::Pipe,
        ..SpawnStdio::default()
    };
    group.spawn(command, stdio)
}

/// Drive `child` as a proxied session over a byte transport.
///
/// Inbound `SessionFrame`s are decoded off `inbound` and applied to the child's
/// stdin; the child's stdout/stderr/exit are encoded onto `outbound`. Returns
/// the child's [`SessionExit`] once it exits, `inbound` reaches EOF, and every
/// outbound frame has been flushed. An `Err` reflects a transport or reap
/// failure, never stdio content.
///
/// `child` must have all three stdio streams piped (use
/// [`spawn_contained_session`]).
pub fn serve_session<C, R, W>(child: C, inbound: R, outbound: W) -> io::Result<SessionExit>
where
    C: SessionChild,
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let (stdin_tx, stdin_rx) = channel::<SessionFrame>();
    let (out_tx, out_rx) = channel::<SessionFrame>();

    // Inbound: decode client→daemon frames and feed them to the pump's stdin
    // channel. When `inbound` hits EOF the thread returns, dropping `stdin_tx`,
    // which lets the pump's stdin thread finish.
    let inbound_thread = thread::spawn(move || decode_inbound(inbound, stdin_tx));
    // Outbound: encode each daemon→client frame and write it to the transport.
    let outbound_thread = thread::spawn(move || encode_outbound(out_rx, outbound));

    let exit = run_child_session(child, out_tx, stdin_rx)?;
    // The pump has sent its terminal Exit frame and dropped `out_tx`; draining
    // threads now finish on their own.
    let _ = inbound_thread.join();
    let outbound_result = outbound_thread.join();
    // Surface a transport write error from the outbound half; a join panic is
    // reported as a broken-session error rather than swallowed.
    match outbound_result {
        Ok(result) => result?,
        Err(_) => return Err(io::Error::other("session outbound thread panicked")),
    }
    Ok(exit)
}

/// Read framed `SessionFrame`s from `inbound` and forward each to `stdin_tx`
/// until EOF (or the pump hangs up). Returns on EOF so the caller's `stdin_tx`
/// drop signals end-of-stdin to the pump.
fn decode_inbound<R: Read>(mut inbound: R, stdin_tx: Sender<SessionFrame>) -> io::Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        // Drain every complete frame currently buffered.
        loop {
            match try_decode_session_frame(&buf) {
                Ok(Some(decoded)) => {
                    buf.drain(..decoded.consumed);
                    if stdin_tx.send(decoded.frame).is_err() {
                        return Ok(()); // pump gone; stop reading
                    }
                }
                Ok(None) => break,
                Err(err) => return Err(io::Error::new(io::ErrorKind::InvalidData, err)),
            }
        }
        match inbound.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(ref err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
}

/// Encode each `SessionFrame` from `out_rx` and write it to `outbound`, flushing
/// per frame so a streaming client sees output promptly. Returns when the pump
/// drops its sender.
fn encode_outbound<W: Write>(out_rx: Receiver<SessionFrame>, mut outbound: W) -> io::Result<()> {
    for (seq, frame) in out_rx.into_iter().enumerate() {
        let wire = encode_session_frame(&frame, seq as u64)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        outbound.write_all(&wire)?;
        outbound.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
