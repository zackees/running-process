//! In-memory registry of daemon-owned pipe-backed sessions
//! (issue #130 milestone 3).
//!
//! Architecture mirrors [`crate::pty_sessions`] but for plain
//! stdin/stdout/stderr pipes instead of a PTY. Each session owns a
//! [`NativeProcess`] (child with three OS pipes) plus a bounded ring buffer
//! per output stream and an optional attached-client mpsc sender per
//! stream. Two reader threads drain stdout / stderr into their buffers and
//! forward to attached clients when present. Stdin is write-only and
//! exposed as an RPC (`WritePipeStdinRequest`) rather than a streaming
//! attach.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use running_process_core::{
    CommandSpec, NativeProcess, ProcessConfig, ProcessError, ReadStatus, StderrMode, StdinMode,
    StreamKind,
};
use tokio::sync::mpsc;
use tracing::debug;

use crate::pty_sessions::{AttachmentEnded, ExitState, OutboundFrame, RingBuffer};

pub const DEFAULT_BACKLOG_BYTES: usize = 1_048_576;
pub const STREAM_CHUNK_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Per-stream state
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PipeStreamSelect {
    Stdout,
    Stderr,
}

impl PipeStreamSelect {
    fn to_stream_kind(self) -> StreamKind {
        match self {
            Self::Stdout => StreamKind::Stdout,
            Self::Stderr => StreamKind::Stderr,
        }
    }
}

struct AttachedStreamClient {
    sender: mpsc::UnboundedSender<OutboundFrame>,
}

struct PipeStreamState {
    backlog: Mutex<RingBuffer>,
    attached: Mutex<Option<AttachedStreamClient>>,
}

impl PipeStreamState {
    fn new() -> Self {
        Self {
            backlog: Mutex::new(RingBuffer::new(DEFAULT_BACKLOG_BYTES)),
            attached: Mutex::new(None),
        }
    }
}

/// Handle returned by [`OwnedPipeSession::attach_stream`]. The streaming
/// server pulls from `receiver` and forwards to the socket as
/// `PipeStreamFrame`.
pub struct PipeAttachmentHandle {
    pub receiver: mpsc::UnboundedReceiver<OutboundFrame>,
}

#[derive(Debug)]
pub enum PipeAttachError {
    AlreadyAttached,
    SessionExited(ExitState),
    StreamUnavailable,
}

// ---------------------------------------------------------------------------
// OwnedPipeSession
// ---------------------------------------------------------------------------

pub struct OwnedPipeSession {
    pub id: String,
    pub process: Arc<NativeProcess>,
    pub pid: u32,
    pub command: String,
    pub cwd: String,
    pub originator: String,
    pub created_at_unix: f64,
    pub merge_stderr_into_stdout: bool,
    stdout: PipeStreamState,
    stderr: PipeStreamState,
    stdin_closed: AtomicBool,
    exit_state: Mutex<Option<ExitState>>,
    reader_shutdown: Arc<AtomicBool>,
    reader_threads: Mutex<Vec<thread::JoinHandle<()>>>,
}

impl OwnedPipeSession {
    fn stream_state(&self, stream: PipeStreamSelect) -> &PipeStreamState {
        match stream {
            PipeStreamSelect::Stdout => &self.stdout,
            PipeStreamSelect::Stderr => &self.stderr,
        }
    }

    pub fn exit_state(&self) -> Option<ExitState> {
        self.exit_state.lock().unwrap().clone()
    }

    pub fn is_attached(&self, stream: PipeStreamSelect) -> bool {
        self.stream_state(stream).attached.lock().unwrap().is_some()
    }

    /// If true, stdout and stderr were merged at spawn time; attempts to
    /// attach to stderr return [`PipeAttachError::StreamUnavailable`].
    pub fn stream_available(&self, stream: PipeStreamSelect) -> bool {
        match stream {
            PipeStreamSelect::Stdout => true,
            PipeStreamSelect::Stderr => !self.merge_stderr_into_stdout,
        }
    }

    pub fn attach_stream(
        &self,
        stream: PipeStreamSelect,
        steal: bool,
    ) -> Result<(PipeAttachmentHandle, Vec<u8>, u64), PipeAttachError> {
        if !self.stream_available(stream) {
            return Err(PipeAttachError::StreamUnavailable);
        }
        if let Some(s) = self.exit_state() {
            return Err(PipeAttachError::SessionExited(s));
        }
        let state = self.stream_state(stream);
        let mut attached = state.attached.lock().unwrap();
        if attached.is_some() {
            if !steal {
                return Err(PipeAttachError::AlreadyAttached);
            }
            if let Some(existing) = attached.take() {
                let _ = existing
                    .sender
                    .send(OutboundFrame::Ended(AttachmentEnded::Stolen));
            }
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let (backlog, dropped) = state.backlog.lock().unwrap().drain();
        *attached = Some(AttachedStreamClient { sender: tx });
        Ok((PipeAttachmentHandle { receiver: rx }, backlog, dropped))
    }

    pub fn clear_attachment(&self, stream: PipeStreamSelect) {
        *self.stream_state(stream).attached.lock().unwrap() = None;
    }

    /// Snapshot the ring-buffer contents for one stream without
    /// consuming them (#130 M7 B4 "sessions log").
    pub fn backlog_snapshot(&self, stream: PipeStreamSelect) -> (Vec<u8>, u64) {
        self.stream_state(stream).backlog.lock().unwrap().snapshot()
    }

    pub fn notify_attached(&self, stream: PipeStreamSelect, frame: OutboundFrame) {
        if let Some(client) = self.stream_state(stream).attached.lock().unwrap().as_ref() {
            let _ = client.sender.send(frame);
        }
    }

    pub fn write_stdin(&self, bytes: &[u8], close_after: bool) -> Result<usize, ProcessError> {
        if self.stdin_closed.load(Ordering::Acquire) {
            return Err(ProcessError::StdinUnavailable);
        }
        if !bytes.is_empty() {
            self.process.write_stdin_streaming(bytes)?;
        }
        if close_after {
            self.process.close_stdin()?;
            self.stdin_closed.store(true, Ordering::Release);
        }
        Ok(bytes.len())
    }

    /// Soft-then-hard termination. M4 will replace this with the
    /// configurable schedule that records the exit path; for M3 the
    /// grace window is observed but the soft signal is just `kill()`
    /// (NativeProcess does not expose a soft-signal API as of this
    /// commit; the hard kill arrives within the grace window anyway).
    pub fn terminate(&self, grace: Duration) -> Result<(), ProcessError> {
        if self.process.poll()?.is_some() {
            return Ok(());
        }
        let process = Arc::clone(&self.process);
        thread::spawn(move || {
            // M4 will issue the soft signal first; for M3 we simply
            // wait the grace window then call kill (which routes through
            // NativeProcess::kill_impl -> std::process::Child::kill on
            // POSIX and Job-Object terminate on Windows).
            thread::sleep(grace);
            if process.poll().ok().flatten().is_none() {
                let _ = process.kill();
            }
        });
        Ok(())
    }

    /// Mark the session for reader shutdown. Subsequent reader iterations
    /// will see this and exit. Public for daemon shutdown path.
    pub fn signal_shutdown(&self) {
        self.reader_shutdown.store(true, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct PipeSessionRegistry {
    sessions: Mutex<HashMap<String, Arc<OwnedPipeSession>>>,
    next_id: AtomicU64,
}

impl PipeSessionRegistry {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<OwnedPipeSession>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    pub fn list(&self) -> Vec<Arc<OwnedPipeSession>> {
        self.sessions.lock().unwrap().values().cloned().collect()
    }

    pub fn remove(&self, id: &str) -> Option<Arc<OwnedPipeSession>> {
        self.sessions.lock().unwrap().remove(id)
    }

    /// Remove every session in the registry that has already exited.
    /// Returns the number of removed entries. Optionally filtered by
    /// originator (empty matches all).
    pub fn purge_exited(&self, originator: &str) -> usize {
        let mut guard = self.sessions.lock().unwrap();
        let to_remove: Vec<String> = guard
            .iter()
            .filter(|(_, s)| {
                s.exit_state().is_some()
                    && (originator.is_empty() || s.originator == originator)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in &to_remove {
            guard.remove(k);
        }
        to_remove.len()
    }

    /// Spawn a new pipe-backed child and register it.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        self: &Arc<Self>,
        argv: Vec<String>,
        cwd: Option<String>,
        env: Option<Vec<(String, String)>>,
        originator: String,
        command_display: String,
        merge_stderr_into_stdout: bool,
    ) -> Result<Arc<OwnedPipeSession>, SpawnError> {
        if argv.is_empty() {
            return Err(SpawnError::EmptyArgv);
        }

        let config = ProcessConfig {
            command: CommandSpec::Argv(argv.clone()),
            cwd: cwd.clone().map(std::path::PathBuf::from),
            env,
            capture: true,
            stderr_mode: if merge_stderr_into_stdout {
                StderrMode::Stdout
            } else {
                StderrMode::Pipe
            },
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Piped,
            nice: None,
        };
        let process = NativeProcess::new(config);
        process
            .start()
            .map_err(|e| SpawnError::Spawn(e.to_string()))?;

        let pid = process.pid().unwrap_or(0);
        let id = self.next_session_id();

        let session = Arc::new(OwnedPipeSession {
            id: id.clone(),
            process: Arc::new(process),
            pid,
            command: command_display,
            cwd: cwd.unwrap_or_default(),
            originator,
            created_at_unix: unix_now(),
            merge_stderr_into_stdout,
            stdout: PipeStreamState::new(),
            stderr: PipeStreamState::new(),
            stdin_closed: AtomicBool::new(false),
            exit_state: Mutex::new(None),
            reader_shutdown: Arc::new(AtomicBool::new(false)),
            reader_threads: Mutex::new(Vec::new()),
        });

        // Spawn reader threads for stdout and stderr.
        let mut handles = Vec::new();
        handles.push(thread::spawn({
            let session = Arc::clone(&session);
            move || reader_loop(session, PipeStreamSelect::Stdout)
        }));
        if !merge_stderr_into_stdout {
            handles.push(thread::spawn({
                let session = Arc::clone(&session);
                move || reader_loop(session, PipeStreamSelect::Stderr)
            }));
        }
        // Spawn exit waiter that sets exit_state once the child exits.
        handles.push(thread::spawn({
            let session = Arc::clone(&session);
            move || exit_waiter_loop(session)
        }));
        *session.reader_threads.lock().unwrap() = handles;

        self.sessions
            .lock()
            .unwrap()
            .insert(id, Arc::clone(&session));
        Ok(session)
    }

    fn next_session_id(&self) -> String {
        let counter = self.next_id.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        format!("pipe-{nanos:016x}-{counter:08x}")
    }
}

impl Default for PipeSessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum SpawnError {
    EmptyArgv,
    Spawn(String),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::EmptyArgv => write!(f, "argv must not be empty"),
            SpawnError::Spawn(s) => write!(f, "failed to spawn pipe session: {s}"),
        }
    }
}

impl std::error::Error for SpawnError {}

// ---------------------------------------------------------------------------
// Reader threads
// ---------------------------------------------------------------------------

fn reader_loop(session: Arc<OwnedPipeSession>, stream: PipeStreamSelect) {
    let stream_kind = stream.to_stream_kind();
    loop {
        if session.reader_shutdown.load(Ordering::Acquire) {
            break;
        }
        match session
            .process
            .read_stream(stream_kind, Some(Duration::from_millis(100)))
        {
            ReadStatus::Line(bytes) if !bytes.is_empty() => {
                let state = session.stream_state(stream);
                state.backlog.lock().unwrap().push(&bytes);
                if let Some(client) = state.attached.lock().unwrap().as_ref() {
                    for slice in bytes.chunks(STREAM_CHUNK_BYTES) {
                        let _ = client.sender.send(OutboundFrame::Output(slice.to_vec()));
                    }
                }
            }
            ReadStatus::Line(_) => {
                // Empty line; skip.
            }
            ReadStatus::Timeout => {
                // No data within the window; loop.
            }
            ReadStatus::Eof => {
                debug!(
                    session_id = %session.id,
                    stream = stream_kind.as_str(),
                    "pipe stream reached EOF"
                );
                // Notify the attached client (if any) and stop reading.
                session.notify_attached(stream, OutboundFrame::Ended(AttachmentEnded::Detached));
                break;
            }
        }
    }
}

fn exit_waiter_loop(session: Arc<OwnedPipeSession>) {
    // Block until exit, then record final state.
    let exit_code = match session.process.wait(None) {
        Ok(code) => code,
        Err(_) => {
            // wait returned an error (NotRunning or similar). Treat as
            // unrecoverable but do not fabricate a code.
            return;
        }
    };
    let state = ExitState {
        exit_code,
        exited_at_unix: unix_now(),
    };
    *session.exit_state.lock().unwrap() = Some(state.clone());
    // Notify any attached stream clients.
    for stream in [PipeStreamSelect::Stdout, PipeStreamSelect::Stderr] {
        if let Some(client) = session
            .stream_state(stream)
            .attached
            .lock()
            .unwrap()
            .take()
        {
            let _ = client.sender.send(OutboundFrame::Exit(state.exit_code));
            let _ = client
                .sender
                .send(OutboundFrame::Ended(AttachmentEnded::SessionExited));
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_assigns_unique_pipe_ids() {
        let r = Arc::new(PipeSessionRegistry::new());
        let a = r.next_session_id();
        let b = r.next_session_id();
        assert_ne!(a, b);
        assert!(a.starts_with("pipe-"));
    }
}
