//! Phase 3 proxy pump (soldr#2365, slice 2b): drive a child process as a
//! broker-proxied compile session.
//!
//! [`run_child_session`] owns a spawned child with piped stdio and bridges it
//! to a pair of [`SessionFrame`] channels: it streams the child's stdout/stderr
//! out as `Stdout`/`Stderr` frames, applies inbound `Stdin`/`StdinEof` frames to
//! the child's stdin, and finishes by sending a terminal `Exit` frame carrying
//! the child's exit code (or signal on Unix).
//!
//! It is deliberately **transport-agnostic** — it speaks in-memory
//! [`std::sync::mpsc`] channels of decoded `SessionFrame`s, not the broker
//! socket. A later Phase 3 slice wraps these channels in the `Frame` envelope on
//! the [`SESSION_PAYLOAD_PROTOCOL`](crate::broker::protocol::SESSION_PAYLOAD_PROTOCOL)
//! lane so the same pump runs across the broker. Keeping the byte-transparency
//! logic here — verified against the direct-execution oracle
//! (`stdio_fidelity_oracle_test`) — means the transport slice only has to prove
//! framing, not fidelity.

use std::io::{Read, Write};
use std::process::{Child, ExitStatus};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::thread;

use crate::broker::protocol_v2::{session_frame, SessionExit, SessionFrame};

/// A sink the pump writes outbound `SessionFrame`s to.
///
/// `send` is **blocking**: a bounded implementation stalls the pump's reader
/// thread when the consumer is slow, which lets the child's OS pipe fill and
/// backpressures the child. That is what keeps a fast producer (rustc) within
/// **bounded per-channel memory** while staying byte-exact — the sink never
/// drops output, it slows the producer (soldr#2365: "bounded per-channel memory
/// asserted", byte-exact fidelity). `Err(())` means the receiver is gone and the
/// pump should stop.
///
/// Implemented for the unbounded [`Sender`] (the handoff / test path, where the
/// consumer drains eagerly) and the bounded [`SyncSender`] (the daemon path,
/// where backpressure is required). The daemon adds an impl for a bounded async
/// channel sender so the same pump feeds an async transport.
pub trait FrameSink: Clone + Send + 'static {
    /// Send one frame, blocking if the sink is bounded and full. On failure the
    /// receiver is gone; the un-sent frame is returned (as `std`'s channels do
    /// via `SendError`) so the caller can stop pumping.
    fn send(&self, frame: SessionFrame) -> Result<(), SessionFrame>;
}

impl FrameSink for Sender<SessionFrame> {
    fn send(&self, frame: SessionFrame) -> Result<(), SessionFrame> {
        Sender::send(self, frame).map_err(|e| e.0)
    }
}

impl FrameSink for SyncSender<SessionFrame> {
    fn send(&self, frame: SessionFrame) -> Result<(), SessionFrame> {
        // Blocks when the bounded channel is full → backpressure to the child.
        SyncSender::send(self, frame).map_err(|e| e.0)
    }
}

/// A spawned child the session pump can drive: the three raw stdio pipes plus a
/// blocking wait that yields a [`SessionExit`].
///
/// Implemented for a plain [`std::process::Child`] (the reference/client path,
/// exercised by the pump's own tests) and, in [`super::session_server`], for the
/// sanitized contained [`crate::spawn::SpawnedChild`] so the daemon can proxy a
/// child confined to its own Job Object / process group. Keeping the pump
/// generic over this trait is what lets one byte-transparent implementation
/// serve both paths without the daemon side re-deriving the fidelity logic.
///
/// The stdio associated types are `Send + 'static` because the pump moves each
/// into its own reader/writer thread.
pub trait SessionChild {
    /// Parent-side writer for the child's stdin.
    type Stdin: Write + Send + 'static;
    /// Parent-side reader for the child's stdout.
    type Stdout: Read + Send + 'static;
    /// Parent-side reader for the child's stderr.
    type Stderr: Read + Send + 'static;

    /// Take the stdin writer. `None` if stdin was not piped or already taken.
    fn take_stdin(&mut self) -> Option<Self::Stdin>;
    /// Take the stdout reader. `None` if stdout was not piped or already taken.
    fn take_stdout(&mut self) -> Option<Self::Stdout>;
    /// Take the stderr reader. `None` if stderr was not piped or already taken.
    fn take_stderr(&mut self) -> Option<Self::Stderr>;
    /// Block until the child exits and report its [`SessionExit`].
    fn wait_session(&mut self) -> std::io::Result<SessionExit>;
}

impl SessionChild for Child {
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
    fn wait_session(&mut self) -> std::io::Result<SessionExit> {
        Ok(session_exit_from_status(&self.wait()?))
    }
}

/// Read `stream` to EOF, emitting each chunk as a `SessionFrame` built by
/// `wrap`. Stops on EOF, a send error (receiver gone), or a read error.
fn pump_output_stream<S, F, K>(stream: Option<S>, wrap: F, out: &K)
where
    S: Read,
    F: Fn(Vec<u8>) -> session_frame::Kind,
    K: FrameSink,
{
    let Some(mut stream) = stream else {
        return;
    };
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let frame = SessionFrame {
                    kind: Some(wrap(buf[..n].to_vec())),
                };
                if out.send(frame).is_err() {
                    break;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// Map a finished child's status to a [`SessionExit`]. On Unix a signal death
/// carries the signal number; on Windows `signal` is always 0.
fn session_exit_from_status(status: &ExitStatus) -> SessionExit {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        SessionExit {
            code: status.code().unwrap_or(-1),
            signal: status.signal().unwrap_or(0),
            metadata: Default::default(),
        }
    }
    #[cfg(windows)]
    {
        SessionExit {
            code: status.code().unwrap_or(-1),
            signal: 0,
            metadata: Default::default(),
        }
    }
}

/// Drive `child` as a proxied session.
///
/// - stdout/stderr are streamed out as `SessionFrame::Stdout`/`Stderr` on `out`,
///   byte-for-byte and on their own streams (never crossed).
/// - inbound `SessionFrame::Stdin(bytes)` are written to the child's stdin;
///   `SessionFrame::StdinEof` closes it (dropping the pipe handle). Other inbound
///   kinds are ignored — only client→daemon frames are meaningful here.
/// - when the child exits, a terminal `SessionFrame::Exit` is sent on `out` and
///   the same [`SessionExit`] is returned.
///
/// `child` must be spawned with all three stdio streams piped. Returns the
/// child's [`SessionExit`]; an `Err` only reflects a failure to reap the child,
/// never stdio content.
pub fn run_child_session<C: SessionChild, K: FrameSink>(
    mut child: C,
    out: K,
    stdin_rx: Receiver<SessionFrame>,
) -> std::io::Result<SessionExit> {
    let child_stdout = child.take_stdout();
    let child_stderr = child.take_stderr();
    let mut child_stdin = child.take_stdin();

    // Apply inbound stdin frames on their own thread so a child that interleaves
    // reads and writes never deadlocks against the output pumps.
    let stdin_handle = thread::spawn(move || {
        for frame in stdin_rx {
            match frame.kind {
                Some(session_frame::Kind::Stdin(bytes)) => {
                    if let Some(writer) = child_stdin.as_mut() {
                        if writer
                            .write_all(&bytes)
                            .and_then(|()| writer.flush())
                            .is_err()
                        {
                            // Child closed its stdin; stop trying to feed it but
                            // keep draining the channel so senders don't block.
                            child_stdin = None;
                        }
                    }
                }
                Some(session_frame::Kind::StdinEof(_)) => {
                    // Drop the write handle → the child sees EOF on stdin.
                    child_stdin = None;
                }
                _ => {}
            }
        }
        // Channel closed (client hung up) without an explicit EOF: still close
        // the child's stdin so a child blocked on read can make progress.
        drop(child_stdin);
    });

    let out_for_stdout = out.clone();
    let stdout_handle = thread::spawn(move || {
        pump_output_stream(child_stdout, session_frame::Kind::Stdout, &out_for_stdout);
    });
    let out_for_stderr = out.clone();
    let stderr_handle = thread::spawn(move || {
        pump_output_stream(child_stderr, session_frame::Kind::Stderr, &out_for_stderr);
    });

    // The output pumps end when the child closes stdout/stderr, i.e. on exit.
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();
    let exit = child.wait_session()?;
    let _ = stdin_handle.join();

    // Clone for the terminal frame and return the original: `SessionExit` is no
    // longer `Copy` (it carries an opaque `metadata` map since soldr#2365 Q3).
    let _ = out.send(SessionFrame {
        kind: Some(session_frame::Kind::Exit(exit.clone())),
    });
    Ok(exit)
}

#[cfg(test)]
mod tests;
