//! Cross-platform process execution, process-tree control, PTY handling, and
//! broker integration primitives.
//!
//! The crate exposes a synchronous process API through [`NativeProcess`], a
//! contained process-group helper through [`ContainedProcessGroup`], low-level
//! spawn helpers through [`spawn()`] and [`spawn_daemon`], and optional
//! daemon/broker modules behind feature flags.

use std::collections::VecDeque;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::observer::ObserverEmitter;

pub mod console_detect;
pub mod containment;
mod helpers;
// Phase 1 of #221: process-observation capability model + portable
// lifecycle baseline. Core-feature-clean (std-only: mpsc + SystemTime),
// so the started/exited baseline is available to the base library
// without pulling in the daemon runtime.
pub mod observer;
#[cfg(feature = "originator-scan")]
pub mod originator;
// Wave 3+4 of #165: proto module + IPC client absorbed from the
// former `running-process-proto` and `running-process-client` crates.
// Both gated behind `feature = "client"`. The protobuf package
// `running_process.daemon.v1` compiles to the file referenced below.
#[cfg(feature = "client")]
/// Prost-generated daemon protocol types used by the client transport.
pub mod proto {
    /// Generated Rust bindings for the `running_process.daemon.v1` protobuf package.
    #[allow(missing_docs)]
    pub mod daemon {
        include!(concat!(env!("OUT_DIR"), "/running_process.daemon.v1.rs"));
    }
}

#[cfg(feature = "client")]
pub mod client;

// Phase 0 of #228: v1 broker module — prost-generated wire types from
// `proto/broker_v1_*.proto`. Gated behind `feature = "client"` because
// prost itself is optional under that feature. Schemas are
// FROZEN FOREVER once v1.0 ships.
#[cfg(feature = "client")]
pub mod broker;

// Phase 1 of #228 (issue #230): maintenance subcommands exposed via
// the `runpm` CLI. Currently just `release-handles` — a cross-platform
// scaffold for the Windows worktree-teardown handle-race fix
// (soldr#710). Gated behind `feature = "client"` because the CLI that
// drives it is.
#[cfg(feature = "client")]
pub mod maintenance;

#[cfg(feature = "client")]
pub mod cleanup;

// Phase 4 of #222 (#427): per-OS boot autostart for the runpm daemon.
// Gated behind `feature = "client"` because the only consumer is the
// `runpm` CLI binary, which is itself client-gated.
#[cfg(feature = "client")]
pub mod boot_autostart;

// Phase 5 of #222 (#428): `runpm.toml` parser used by the `runpm` CLI
// to batch-start `[[app]]` entries. Lives in the library (not under
// `src/bin/`) so the integration test in `tests/runpm_toml_config.rs`
// can drive the same code path the binary uses.
#[cfg(feature = "client")]
pub mod runpm_config;

// #415: consumer-consumable conformance test kit. Gated behind the
// off-by-default `test-support` cargo feature (which implies `client`)
// so the helpers ship in the published crate but only compile when a
// consumer opts in as a dev-dependency.
#[cfg(feature = "test-support")]
pub mod test_support;

// Lightweight tee sink primitives for callers that want transcript/log
// fan-out without pulling in the full daemon runtime.
#[cfg(feature = "telemetry")]
#[path = "daemon/telemetry.rs"]
pub mod telemetry;

// Wave 5 of #165: daemon runtime absorbed from `running-process-daemon`.
// Heavy deps (tokio, sqlite, etc.) gated behind `feature = "daemon"`.
#[cfg(feature = "daemon")]
/// Daemon runtime APIs and helpers enabled by the `daemon` feature.
pub mod daemon;
#[cfg(feature = "pty")]
/// PTY-backed process APIs.
pub mod pty;
mod public_symbols;
mod rust_debug;
pub mod spawn;
pub mod systemd_killmode;
pub mod terminal_graphics;
mod types;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

pub use console_detect::{monitor_console_windows, ConsoleWindowInfo};
pub use containment::{ContainedProcessGroup, ORIGINATOR_ENV_VAR};
pub use observer::{
    CapabilitySupport, CategoryCapability, EventCategory, ObserverCapabilities, ObserverConfig,
    ObserverEvent, ObserverEventKind, ObserverSubscriber,
};
#[cfg(feature = "originator-scan")]
pub use originator::{find_processes_by_originator, OriginatorProcessInfo};
pub use rust_debug::{render_rust_debug_traces, RustDebugScopeGuard};
pub use spawn::{
    spawn, spawn_daemon, spawn_daemon_with_clear_env, DaemonChild, SpawnStdio, SpawnedChild,
    StdioSource,
};
pub use terminal_graphics::{
    current_terminal_capabilities, current_terminal_capabilities_with_timeout,
    detect_terminal_capabilities, CapabilityStatus, EvidenceStrength, GraphicsCapability,
    GraphicsProtocol, TerminalCapabilities, TerminalCapabilityInput, TerminalGraphicsCapabilities,
    TerminalProbeEvidence,
};
pub use types::{
    CommandSpec, ProcessConfig, ProcessError, ReadStatus, RunOutput, StderrMode, StdinMode,
    StreamEvent, StreamKind,
};

pub(crate) use helpers::{exit_code, feed_chunk, kill_drain_deadline, log_spawned_child_pid};
#[cfg(unix)]
pub use unix::{unix_set_priority, unix_signal_process, unix_signal_process_group, UnixSignal};
#[cfg(windows)]
pub(crate) use windows::{
    assign_child_to_windows_kill_on_close_job_impl, windows_creation_flags, CapturePipeHandles,
    WindowsJobHandle,
};

#[macro_export]
/// Create a scoped Rust debug trace label for the current function body.
macro_rules! rp_rust_debug_scope {
    ($label:expr) => {
        let _running_process_rust_debug_scope =
            $crate::RustDebugScopeGuard::enter($label, file!(), line!());
    };
}

#[derive(Default)]
struct QueueState {
    stdout_queue: VecDeque<Vec<u8>>,
    stderr_queue: VecDeque<Vec<u8>>,
    combined_queue: VecDeque<StreamEvent>,
    stdout_history: VecDeque<Vec<u8>>,
    stderr_history: VecDeque<Vec<u8>>,
    combined_history: VecDeque<StreamEvent>,
    stdout_raw: Vec<u8>,
    stderr_raw: Vec<u8>,
    stdout_history_bytes: usize,
    stderr_history_bytes: usize,
    combined_history_bytes: usize,
    stdout_closed: bool,
    stderr_closed: bool,
}

/// Sentinel value for returncode atomic: process has not exited yet.
const RETURNCODE_NOT_SET: i64 = i64::MIN;

struct SharedState {
    queues: Mutex<QueueState>,
    condvar: Condvar,
    /// Atomic exit code. `RETURNCODE_NOT_SET` means "not exited yet".
    /// Updated by a background waiter thread — reading is lock-free.
    returncode: AtomicI64,
    /// Phase 1 of #221: optional lifecycle-event emitter. `None` means
    /// observation is off (the off-by-default path), so the lifecycle
    /// hooks are inert. When `Some`, `started` is emitted once at spawn
    /// and `exited` exactly once on the first returncode transition.
    observer: Option<ObserverEmitter>,
    /// Guards against emitting more than one `exited` event when several
    /// code paths (waiter thread, `poll`, `kill`) race to record the exit.
    observer_exit_emitted: AtomicBool,
}

struct ChildState {
    child: Child,
    #[cfg(windows)]
    _job: WindowsJobHandle,
}

impl SharedState {
    fn new(capture: bool) -> Self {
        Self::with_observer(capture, None)
    }

    fn with_observer(capture: bool, observer: Option<ObserverEmitter>) -> Self {
        let queues = QueueState {
            stdout_closed: !capture,
            stderr_closed: !capture,
            ..QueueState::default()
        };
        Self {
            queues: Mutex::new(queues),
            condvar: Condvar::new(),
            returncode: AtomicI64::new(RETURNCODE_NOT_SET),
            observer,
            observer_exit_emitted: AtomicBool::new(false),
        }
    }

    /// Emit the lifecycle `exited` event exactly once, regardless of which
    /// code path first observes the exit. No-op when observation is off.
    fn emit_exited(&self, pid: u32, exit_code: i32) {
        let Some(emitter) = self.observer.as_ref() else {
            return;
        };
        if self
            .observer_exit_emitted
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            emitter.emit_exited(pid, exit_code);
        }
    }
}

/// A cross-platform child process with optional output capture.
///
/// `NativeProcess` wraps [`std::process::Command`] with the crate's
/// process-tree containment, capture draining, timeout, and terminal-control
/// behavior. Methods are synchronous and are safe to call from ordinary
/// blocking code.
pub struct NativeProcess {
    config: ProcessConfig,
    child: Arc<Mutex<Option<ChildState>>>,
    shared: Arc<SharedState>,
    #[cfg(windows)]
    capture_pipe_handles: Arc<Mutex<CapturePipeHandles>>,
}

impl NativeProcess {
    /// Create a process wrapper from a [`ProcessConfig`].
    ///
    /// The child is not spawned until [`Self::start`] is called. Process
    /// observation is **off by default**: no lifecycle events are emitted
    /// unless [`Self::with_observer`] is used instead.
    pub fn new(config: ProcessConfig) -> Self {
        Self::new_with_observer(config, None)
    }

    /// Create a process wrapper with process observation enabled (Phase 1
    /// of #221).
    ///
    /// Returns the wrapper paired with an [`ObserverSubscriber`] that
    /// receives a [`started`](crate::ObserverEventKind::Started) event when
    /// [`Self::start`] spawns the child and exactly one
    /// [`exited`](crate::ObserverEventKind::Exited) event when the child is
    /// reaped — for the categories the `config` requests that are actually
    /// `Supported` (only [`Lifecycle`](crate::EventCategory::Lifecycle) in
    /// Phase 1; see [`ObserverCapabilities::negotiate`](crate::ObserverCapabilities::negotiate)).
    ///
    /// The emitter never blocks on a slow or dropped subscriber.
    pub fn with_observer(
        config: ProcessConfig,
        observer: crate::observer::ObserverConfig,
    ) -> (Self, ObserverSubscriber) {
        let (emitter, subscriber) = ObserverEmitter::new(observer);
        let process = Self::new_with_observer(config, Some(emitter));
        (process, subscriber)
    }

    fn new_with_observer(config: ProcessConfig, observer: Option<ObserverEmitter>) -> Self {
        let shared = match observer {
            // Off-by-default path: no emitter, no observation state.
            None => SharedState::new(config.capture),
            Some(emitter) => SharedState::with_observer(config.capture, Some(emitter)),
        };
        Self {
            shared: Arc::new(shared),
            child: Arc::new(Mutex::new(None)),
            config,
            #[cfg(windows)]
            capture_pipe_handles: Arc::new(Mutex::new(CapturePipeHandles::default())),
        }
    }

    // Preserve a stable Rust frame here in release user dumps.
    #[inline(never)]
    /// Spawn the configured child process.
    ///
    /// Returns [`ProcessError::AlreadyStarted`] if the same wrapper already
    /// owns a running child.
    pub fn start(&self) -> Result<(), ProcessError> {
        public_symbols::rp_native_process_start_public(self)
    }

    fn start_impl(&self) -> Result<(), ProcessError> {
        crate::rp_rust_debug_scope!("running_process::NativeProcess::start");
        let mut guard = self.child.lock().expect("child mutex poisoned");
        if guard.is_some() {
            return Err(ProcessError::AlreadyStarted);
        }

        let mut command = self.build_command();
        match self.config.stdin_mode {
            StdinMode::Inherit => {}
            StdinMode::Piped => {
                command.stdin(Stdio::piped());
            }
            StdinMode::Null => {
                command.stdin(Stdio::null());
            }
        }
        if self.config.capture {
            command.stdout(Stdio::piped());
            command.stderr(Stdio::piped());
        }

        let mut child = command.spawn().map_err(ProcessError::Spawn)?;
        log_spawned_child_pid(child.id()).map_err(ProcessError::Spawn)?;
        // Phase 1 of #221: emit the lifecycle `started` event. No-op when
        // observation is off (the common, off-by-default path).
        if let Some(emitter) = self.shared.observer.as_ref() {
            emitter.emit_started(child.id());
        }
        // #539 slice 2: when the observer requests EventCategory::Process,
        // associate an IOCP with the per-spawn Job Object so a pump thread
        // can forward descendant lifecycle events. The Lifecycle category
        // is still served by emit_started / emit_exited above and below.
        #[cfg(windows)]
        let job = {
            let descendant_sink = self
                .shared
                .observer
                .as_ref()
                .and_then(|e| e.descendant_sink());
            let direct_pid = child.id();
            public_symbols::rp_assign_child_to_windows_kill_on_close_job_with_observer_public(
                &child,
                descendant_sink,
                direct_pid,
            )
            .map_err(ProcessError::Spawn)?
        };
        // #539 slice 5: Linux descendant lifecycle via PR_SET_CHILD_SUBREAPER
        // + /proc polling pump. No-admin, polling-based — see
        // observer::descendants_linux module docs for tradeoffs.
        #[cfg(target_os = "linux")]
        {
            if let Some(emitter) = self.shared.observer.as_ref() {
                if let Some(sink) = emitter.descendant_sink() {
                    crate::observer::descendants_linux::enable_subreaper();
                    crate::observer::descendants_linux::spawn_pump(child.id(), sink);
                }
            }
        }
        // #539 slice 7: macOS descendant lifecycle via kqueue + EVFILT_PROC
        // + NOTE_TRACK. Fully event-driven (no polling) — see
        // observer::descendants_macos module docs for tradeoffs.
        #[cfg(target_os = "macos")]
        {
            if let Some(emitter) = self.shared.observer.as_ref() {
                if let Some(sink) = emitter.descendant_sink() {
                    crate::observer::descendants_macos::spawn_pump(child.id(), sink);
                }
            }
        }
        if self.config.capture {
            let stdout = child.stdout.take().expect("stdout pipe missing");
            let stderr = child.stderr.take().expect("stderr pipe missing");
            #[cfg(windows)]
            {
                use std::os::windows::io::AsRawHandle;
                let mut handles = self
                    .capture_pipe_handles
                    .lock()
                    .expect("capture pipe handles mutex poisoned");
                handles.stdout = Some(stdout.as_raw_handle() as usize);
                handles.stderr = Some(stderr.as_raw_handle() as usize);
            }
            self.spawn_reader(
                stdout,
                StreamKind::Stdout,
                StreamKind::Stdout,
                self.pipe_done_callback(StreamKind::Stdout),
            );
            self.spawn_reader(
                stderr,
                StreamKind::Stderr,
                match self.config.stderr_mode {
                    StderrMode::Stdout => StreamKind::Stdout,
                    StderrMode::Pipe => StreamKind::Stderr,
                },
                self.pipe_done_callback(StreamKind::Stderr),
            );
        }
        *guard = Some(ChildState {
            child,
            #[cfg(windows)]
            _job: job,
        });
        drop(guard);
        self.spawn_exit_waiter();
        Ok(())
    }

    /// Background thread that polls for process exit and stores the exit code
    /// atomically. This makes `returncode` auto-update without explicit `poll()`.
    fn spawn_exit_waiter(&self) {
        let child = Arc::clone(&self.child);
        let shared = Arc::clone(&self.shared);
        let capture = self.config.capture;
        #[cfg(windows)]
        let capture_pipe_handles = Arc::clone(&self.capture_pipe_handles);
        thread::spawn(move || {
            loop {
                if shared.returncode.load(Ordering::Acquire) != RETURNCODE_NOT_SET {
                    return;
                }
                let exited = {
                    let mut guard = child.lock().expect("child mutex poisoned");
                    if let Some(child_state) = guard.as_mut() {
                        let pid = child_state.child.id();
                        match child_state.child.try_wait() {
                            Ok(Some(status)) => {
                                let code = exit_code(status);
                                shared.returncode.store(code as i64, Ordering::Release);
                                // Phase 1 of #221: lifecycle `exited`. Emit
                                // before notifying waiters and is guarded so
                                // only the first exit-observer fires.
                                shared.emit_exited(pid, code);
                                shared.condvar.notify_all();
                                true
                            }
                            Ok(None) => false,
                            Err(_) => return,
                        }
                    } else {
                        return;
                    }
                };
                if exited {
                    // The direct child has exited. Bound the capture-completion
                    // wait so wait()/close()/read_* on the natural-exit path
                    // cannot wedge forever when a grandchild inherited the pipe
                    // and outlives the child (issue #590, cluster A). Unlike
                    // `kill_impl` we do NOT cancel the reader up front: a
                    // short-lived grandchild may still emit output the caller
                    // expects to capture, so the reader is left to drain
                    // naturally within the grace window. Only if the window
                    // elapses with the pipe still held open do we cancel, to
                    // release the otherwise-leaked reader thread (Windows:
                    // CancelIoEx). The child lock is released before this
                    // potentially-blocking finalize so poll()/kill() are never
                    // held off.
                    if capture {
                        let drained = finalize_capture_completion(&shared, kill_drain_deadline());
                        #[cfg(windows)]
                        if !drained {
                            cancel_capture_pipe_io(&capture_pipe_handles);
                        }
                        #[cfg(not(windows))]
                        {
                            let _ = drained;
                        }
                    }
                    return;
                }
                // #199: intentional — capture thread polling for
                // child-exit. `try_wait` is non-blocking by design;
                // we can't block here because the thread also drains
                // pipe state alongside the exit check. 10ms keeps the
                // CPU cost negligible while staying responsive.
                thread::sleep(Duration::from_millis(10));
            }
        });
    }

    /// Write bytes to the child's stdin and then close stdin.
    pub fn write_stdin(&self, data: &[u8]) -> Result<(), ProcessError> {
        let mut guard = self.child.lock().expect("child mutex poisoned");
        let child = &mut guard.as_mut().ok_or(ProcessError::NotRunning)?.child;
        let stdin = child.stdin.as_mut().ok_or(ProcessError::StdinUnavailable)?;
        use std::io::Write;
        stdin.write_all(data).map_err(ProcessError::Io)?;
        stdin.flush().map_err(ProcessError::Io)?;
        drop(child.stdin.take());
        Ok(())
    }

    /// Write to the child's stdin without closing it afterwards, so the
    /// caller can issue additional writes. Used by interactive
    /// pipe-backed sessions (#130 milestone 3) where the daemon keeps
    /// stdin open across multiple client input frames.
    pub fn write_stdin_streaming(&self, data: &[u8]) -> Result<(), ProcessError> {
        let mut guard = self.child.lock().expect("child mutex poisoned");
        let child = &mut guard.as_mut().ok_or(ProcessError::NotRunning)?.child;
        let stdin = child.stdin.as_mut().ok_or(ProcessError::StdinUnavailable)?;
        use std::io::Write;
        stdin.write_all(data).map_err(ProcessError::Io)?;
        stdin.flush().map_err(ProcessError::Io)?;
        Ok(())
    }

    /// Explicitly close the child's stdin (signals EOF to the child).
    /// Idempotent: returns Ok if stdin was already closed.
    pub fn close_stdin(&self) -> Result<(), ProcessError> {
        let mut guard = self.child.lock().expect("child mutex poisoned");
        let child = &mut guard.as_mut().ok_or(ProcessError::NotRunning)?.child;
        drop(child.stdin.take());
        Ok(())
    }

    /// Check whether the child has exited without blocking.
    ///
    /// Returns `Ok(None)` while the process is still running.
    pub fn poll(&self) -> Result<Option<i32>, ProcessError> {
        // Fast path: check atomic set by background waiter thread.
        if let Some(code) = self.returncode() {
            return Ok(Some(code));
        }
        let mut guard = self.child.lock().expect("child mutex poisoned");
        let Some(child_state) = guard.as_mut() else {
            return Ok(self.returncode());
        };
        let pid = child_state.child.id();
        let child = &mut child_state.child;
        let status = child.try_wait().map_err(ProcessError::Io)?;
        if let Some(status) = status {
            let code = exit_code(status);
            self.set_returncode(code);
            self.shared.emit_exited(pid, code);
            return Ok(Some(code));
        }
        Ok(None)
    }

    // Preserve a stable Rust frame here in release user dumps.
    #[inline(never)]
    /// Wait for the child to exit.
    ///
    /// When `timeout` is `Some`, returns [`ProcessError::Timeout`] if the
    /// child does not exit before the duration elapses.
    pub fn wait(&self, timeout: Option<Duration>) -> Result<i32, ProcessError> {
        public_symbols::rp_native_process_wait_public(self, timeout)
    }

    fn wait_impl(&self, timeout: Option<Duration>) -> Result<i32, ProcessError> {
        crate::rp_rust_debug_scope!("running_process::NativeProcess::wait");
        if self.child.lock().expect("child mutex poisoned").is_none() {
            return self.returncode().ok_or(ProcessError::NotRunning);
        }
        // Fast path: already exited.
        if let Some(code) = self.returncode() {
            self.finish_capture_drain();
            return Ok(code);
        }
        let start = Instant::now();
        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");
        loop {
            // Check returncode (set by exit-waiter thread via atomic + condvar).
            let rc = self.shared.returncode.load(Ordering::Acquire);
            if rc != RETURNCODE_NOT_SET {
                drop(guard);
                let code = rc as i32;
                self.finish_capture_drain();
                return Ok(code);
            }
            if let Some(limit) = timeout {
                let elapsed = start.elapsed();
                if elapsed >= limit {
                    return Err(ProcessError::Timeout);
                }
                let remaining = limit - elapsed;
                // Wait on condvar with timeout, capped at 50ms to recheck.
                let wait_time = remaining.min(Duration::from_millis(50));
                guard = self
                    .shared
                    .condvar
                    .wait_timeout(guard, wait_time)
                    .expect("queue mutex poisoned")
                    .0;
            } else {
                // Wait on condvar with periodic recheck.
                guard = self
                    .shared
                    .condvar
                    .wait_timeout(guard, Duration::from_millis(50))
                    .expect("queue mutex poisoned")
                    .0;
            }
        }
    }

    // Preserve a stable Rust frame here in release user dumps.
    #[inline(never)]
    /// Forcefully terminate the child process.
    pub fn kill(&self) -> Result<(), ProcessError> {
        public_symbols::rp_native_process_kill_public(self)
    }

    fn kill_impl(&self) -> Result<(), ProcessError> {
        crate::rp_rust_debug_scope!("running_process::NativeProcess::kill");
        {
            let mut guard = self.child.lock().expect("child mutex poisoned");
            let child = &mut guard.as_mut().ok_or(ProcessError::NotRunning)?.child;
            let pid = child.id();
            child.kill().map_err(ProcessError::Io)?;
            let status = child.wait().map_err(ProcessError::Io)?;
            let code = exit_code(status);
            self.set_returncode(code);
            // Phase 1 of #221: a killed child still produces a lifecycle
            // `exited` event (guarded against double-emit by the waiter).
            self.shared.emit_exited(pid, code);
        }
        // On Windows, interrupt any pending blocking `read()` in the
        // per-stream reader threads so they fall out of their loops
        // immediately. This is what makes the grandchild-pipe-orphan
        // case (FastLED Bug B: uv.exe spawns a python.exe grandchild
        // that inherits the pipe and outlives uv) wake up in
        // microseconds instead of waiting for the bounded-drain
        // safety-net deadline below.
        #[cfg(windows)]
        self.cancel_capture_io();
        // Synchronize with the per-stream reader threads so that by the
        // time kill() returns, the capture queues have flipped from
        // "blocked on read" to "closed" and downstream pollers (e.g.
        // take_combined_line) observe EOS instead of timeout. Without
        // this, callers that hit a wait()-timeout path see Python code
        // raise TimeoutError, kill the child, then race the reader
        // threads — a 10ms poll loop can miss the EOS flip entirely.
        //
        // The deadline is the safety-net: on Windows `CancelIoEx`
        // above almost always wakes the readers first; on POSIX, and
        // in any pathological Windows case where `CancelIoEx` doesn't
        // fire, the deadline guarantees `kill()` still returns.
        public_symbols::rp_native_process_wait_for_capture_completion_with_deadline_public(
            self,
            kill_drain_deadline(),
        );
        Ok(())
    }

    /// Terminate the child process.
    ///
    /// This currently uses the same hard-kill path as [`Self::kill`].
    pub fn terminate(&self) -> Result<(), ProcessError> {
        self.kill()
    }

    /// Send the OS-appropriate soft termination signal to the child's
    /// process group (POSIX: SIGTERM to `-pid`; Windows: no soft path
    /// implemented yet — returns Ok without doing anything so callers
    /// can run the same code on both platforms and rely on the post-
    /// grace hard kill).
    ///
    /// Requires `ProcessConfig.create_process_group=true` on POSIX so
    /// that `-pid` resolves to the child's own group. With the default
    /// `create_process_group=false`, the kill would walk back to the
    /// caller's group; the method silently no-ops in that case to avoid
    /// signaling the wrong tree.
    ///
    /// Used by the daemon-side pipe sessions (#130 M4 follow-up) so
    /// that `TerminationOutcome::SoftExit` becomes meaningful on POSIX.
    pub fn terminate_group_soft(&self) -> Result<(), ProcessError> {
        #[cfg(unix)]
        {
            if !self.config.create_process_group {
                return Ok(());
            }
            let pid = match self.pid() {
                Some(p) => p as i32,
                None => return Err(ProcessError::NotRunning),
            };
            let result = unsafe { libc::kill(-pid, libc::SIGTERM) };
            if result != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::ESRCH) {
                    return Err(ProcessError::Io(err));
                }
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            if !self.config.create_process_group {
                // GenerateConsoleCtrlEvent only routes to children
                // spawned with CREATE_NEW_PROCESS_GROUP, and the
                // event would otherwise hit the daemon's own group.
                // No-op so the hard-kill schedule still wins.
                return Ok(());
            }
            let pid = match self.pid() {
                Some(p) => p,
                None => return Err(ProcessError::NotRunning),
            };
            // GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT=1, pid).
            // SAFETY: the FFI call is the standard Windows API; no
            // borrowed Rust state is involved.
            let ok = unsafe {
                winapi::um::wincon::GenerateConsoleCtrlEvent(
                    winapi::um::wincon::CTRL_BREAK_EVENT,
                    pid,
                )
            };
            if ok == 0 {
                let err = std::io::Error::last_os_error();
                // ERROR_INVALID_HANDLE means the child has already
                // exited or has detached from the console — treat as
                // success because the soft step's only goal is to
                // give the child a chance to exit cleanly, and a
                // dead/detached child does not need one.
                if err.raw_os_error() != Some(6) {
                    return Err(ProcessError::Io(err));
                }
            }
            Ok(())
        }
    }

    // Preserve a stable Rust frame here in release user dumps.
    #[inline(never)]
    /// Close the process wrapper by terminating the child when it is running.
    pub fn close(&self) -> Result<(), ProcessError> {
        public_symbols::rp_native_process_close_public(self)
    }

    fn close_impl(&self) -> Result<(), ProcessError> {
        crate::rp_rust_debug_scope!("running_process::NativeProcess::close");
        if self.child.lock().expect("child mutex poisoned").is_none() {
            return Ok(());
        }
        if self.poll()?.is_none() {
            self.kill()?;
        } else {
            self.finish_capture_drain();
        }
        Ok(())
    }

    /// Return the child process id when the wrapper currently owns a child.
    pub fn pid(&self) -> Option<u32> {
        self.child
            .lock()
            .expect("child mutex poisoned")
            .as_ref()
            .map(|state| state.child.id())
    }

    /// Return the cached exit code when the child has exited.
    pub fn returncode(&self) -> Option<i32> {
        let v = self.shared.returncode.load(Ordering::Acquire);
        if v == RETURNCODE_NOT_SET {
            None
        } else {
            Some(v as i32)
        }
    }

    /// Return whether captured output is queued for one stream.
    pub fn has_pending_stream(&self, stream: StreamKind) -> bool {
        if stream == StreamKind::Stderr && self.config.stderr_mode == StderrMode::Stdout {
            return false;
        }
        let guard = self.shared.queues.lock().expect("queue mutex poisoned");
        match stream {
            StreamKind::Stdout => !guard.stdout_queue.is_empty(),
            StreamKind::Stderr => !guard.stderr_queue.is_empty(),
        }
    }

    /// Return whether captured combined output is queued.
    pub fn has_pending_combined(&self) -> bool {
        let guard = self.shared.queues.lock().expect("queue mutex poisoned");
        !guard.combined_queue.is_empty()
    }

    /// Drain and return all queued output for one stream.
    pub fn drain_stream(&self, stream: StreamKind) -> Vec<Vec<u8>> {
        if stream == StreamKind::Stderr && self.config.stderr_mode == StderrMode::Stdout {
            return Vec::new();
        }
        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");
        let queue = match stream {
            StreamKind::Stdout => &mut guard.stdout_queue,
            StreamKind::Stderr => &mut guard.stderr_queue,
        };
        queue.drain(..).collect()
    }

    /// Drain and return all queued combined output events.
    pub fn drain_combined(&self) -> Vec<StreamEvent> {
        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");
        guard.combined_queue.drain(..).collect()
    }

    /// Read the next captured chunk from one stream.
    ///
    /// Returns [`ReadStatus::Timeout`] when `timeout` elapses before output or
    /// EOF is observed.
    pub fn read_stream(
        &self,
        stream: StreamKind,
        timeout: Option<Duration>,
    ) -> ReadStatus<Vec<u8>> {
        let deadline = timeout.map(|limit| Instant::now() + limit);
        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");

        loop {
            if stream == StreamKind::Stderr && self.config.stderr_mode == StderrMode::Stdout {
                return ReadStatus::Eof;
            }

            let queue = match stream {
                StreamKind::Stdout => &mut guard.stdout_queue,
                StreamKind::Stderr => &mut guard.stderr_queue,
            };
            if let Some(line) = queue.pop_front() {
                return ReadStatus::Line(line);
            }

            let closed = match stream {
                StreamKind::Stdout => {
                    if self.config.stderr_mode == StderrMode::Stdout {
                        guard.stdout_closed && guard.stderr_closed
                    } else {
                        guard.stdout_closed
                    }
                }
                StreamKind::Stderr => guard.stderr_closed,
            };
            if closed {
                return ReadStatus::Eof;
            }

            match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return ReadStatus::Timeout;
                    }
                    let wait = deadline.saturating_duration_since(now);
                    let result = self
                        .shared
                        .condvar
                        .wait_timeout(guard, wait)
                        .expect("queue mutex poisoned");
                    guard = result.0;
                    if result.1.timed_out() {
                        return ReadStatus::Timeout;
                    }
                }
                None => {
                    guard = self
                        .shared
                        .condvar
                        .wait(guard)
                        .expect("queue mutex poisoned");
                }
            }
        }
    }

    // Preserve a stable Rust frame here in release user dumps.
    #[inline(never)]
    /// Read the next captured combined stream event.
    pub fn read_combined(&self, timeout: Option<Duration>) -> ReadStatus<StreamEvent> {
        public_symbols::rp_native_process_read_combined_public(self, timeout)
    }

    fn read_combined_impl(&self, timeout: Option<Duration>) -> ReadStatus<StreamEvent> {
        crate::rp_rust_debug_scope!("running_process::NativeProcess::read_combined");
        let deadline = timeout.map(|limit| Instant::now() + limit);
        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");

        loop {
            if let Some(event) = guard.combined_queue.pop_front() {
                return ReadStatus::Line(event);
            }
            if guard.stdout_closed && guard.stderr_closed {
                return ReadStatus::Eof;
            }

            match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return ReadStatus::Timeout;
                    }
                    let wait = deadline.saturating_duration_since(now);
                    let result = self
                        .shared
                        .condvar
                        .wait_timeout(guard, wait)
                        .expect("queue mutex poisoned");
                    guard = result.0;
                    if result.1.timed_out() {
                        return ReadStatus::Timeout;
                    }
                }
                None => {
                    guard = self
                        .shared
                        .condvar
                        .wait(guard)
                        .expect("queue mutex poisoned");
                }
            }
        }
    }

    /// Return the retained stdout history.
    pub fn captured_stdout(&self) -> Vec<Vec<u8>> {
        self.shared
            .queues
            .lock()
            .expect("queue mutex poisoned")
            .stdout_history
            .clone()
            .into_iter()
            .collect()
    }

    fn captured_stdout_raw(&self) -> Vec<u8> {
        self.shared
            .queues
            .lock()
            .expect("queue mutex poisoned")
            .stdout_raw
            .clone()
    }

    /// Return the retained stderr history.
    pub fn captured_stderr(&self) -> Vec<Vec<u8>> {
        if self.config.stderr_mode == StderrMode::Stdout {
            return Vec::new();
        }
        self.shared
            .queues
            .lock()
            .expect("queue mutex poisoned")
            .stderr_history
            .clone()
            .into_iter()
            .collect()
    }

    fn captured_stderr_raw(&self) -> Vec<u8> {
        if self.config.stderr_mode == StderrMode::Stdout {
            return Vec::new();
        }
        self.shared
            .queues
            .lock()
            .expect("queue mutex poisoned")
            .stderr_raw
            .clone()
    }

    /// Return the retained combined stdout/stderr event history.
    pub fn captured_combined(&self) -> Vec<StreamEvent> {
        self.shared
            .queues
            .lock()
            .expect("queue mutex poisoned")
            .combined_history
            .clone()
            .into_iter()
            .collect()
    }

    /// Return the retained byte count for one captured stream.
    pub fn captured_stream_bytes(&self, stream: StreamKind) -> usize {
        if stream == StreamKind::Stderr && self.config.stderr_mode == StderrMode::Stdout {
            return 0;
        }
        let guard = self.shared.queues.lock().expect("queue mutex poisoned");
        match stream {
            StreamKind::Stdout => guard.stdout_history_bytes,
            StreamKind::Stderr => guard.stderr_history_bytes,
        }
    }

    /// Return the retained byte count for combined captured output.
    pub fn captured_combined_bytes(&self) -> usize {
        self.shared
            .queues
            .lock()
            .expect("queue mutex poisoned")
            .combined_history_bytes
    }

    /// Clear retained output history for one stream and return freed bytes.
    pub fn clear_captured_stream(&self, stream: StreamKind) -> usize {
        if stream == StreamKind::Stderr && self.config.stderr_mode == StderrMode::Stdout {
            return 0;
        }
        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");
        match stream {
            StreamKind::Stdout => {
                let released = guard.stdout_history_bytes;
                guard.stdout_history.clear();
                guard.stdout_raw.clear();
                guard.stdout_history_bytes = 0;
                released
            }
            StreamKind::Stderr => {
                let released = guard.stderr_history_bytes;
                guard.stderr_history.clear();
                guard.stderr_raw.clear();
                guard.stderr_history_bytes = 0;
                released
            }
        }
    }

    /// Clear retained combined output history and return freed bytes.
    pub fn clear_captured_combined(&self) -> usize {
        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");
        let released = guard.combined_history_bytes;
        guard.combined_history.clear();
        guard.combined_history_bytes = 0;
        released
    }

    fn build_command(&self) -> Command {
        let mut command = match &self.config.command {
            CommandSpec::Shell(command) => shell_command(command),
            CommandSpec::Argv(argv) => {
                let mut command = Command::new(&argv[0]);
                if argv.len() > 1 {
                    command.args(&argv[1..]);
                }
                command
            }
        };
        if let Some(cwd) = &self.config.cwd {
            command.current_dir(cwd);
        }
        if let Some(env) = &self.config.env {
            command.env_clear();
            command.envs(env.iter().map(|(k, v)| (k, v)));
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;

            // #584: defaults to CREATE_NO_WINDOW so a console child spawned
            // by the window-less daemon does not flash a console window,
            // while preserving the caller's console opinion, priority, and
            // CREATE_NEW_PROCESS_GROUP bits. See `windows_creation_flags`.
            let flags = windows_creation_flags(
                self.config.creationflags,
                self.config.create_process_group,
                self.config.nice,
            );
            if flags != 0 {
                command.creation_flags(flags);
            }
        }
        #[cfg(unix)]
        {
            let create_process_group = self.config.create_process_group;
            let nice = self.config.nice;

            if create_process_group || nice.is_some() {
                use std::os::unix::process::CommandExt;

                unsafe {
                    command.pre_exec(move || {
                        if create_process_group && libc::setpgid(0, 0) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        if let Some(nice) = nice {
                            let result = libc::setpriority(libc::PRIO_PROCESS, 0, nice);
                            if result == -1 {
                                return Err(std::io::Error::last_os_error());
                            }
                        }
                        Ok(())
                    });
                }
            }
        }
        command
    }

    fn spawn_reader<R>(
        &self,
        pipe: R,
        source_stream: StreamKind,
        visible_stream: StreamKind,
        on_pipe_done: Box<dyn FnOnce() + Send>,
    ) where
        R: Read + Send + 'static,
    {
        let shared = Arc::clone(&self.shared);
        thread::spawn(move || {
            let mut reader = pipe;
            let mut chunk = vec![0_u8; 65536];
            let mut pending = Vec::new();

            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        append_raw(&shared, visible_stream, &chunk[..n]);
                        let lines = feed_chunk(&mut pending, &chunk[..n]);
                        emit_lines(&shared, visible_stream, lines);
                    }
                    Err(_) => break,
                }
            }

            if !pending.is_empty() {
                emit_lines(&shared, visible_stream, vec![std::mem::take(&mut pending)]);
            }

            // Clear the parent-side pipe-handle slot under its mutex
            // before dropping the reader. After this returns,
            // `kill_impl` can no longer try to `CancelIoEx` on us, so
            // it's safe for `reader`'s drop to close the HANDLE.
            on_pipe_done();
            drop(reader);

            let mut guard = shared.queues.lock().expect("queue mutex poisoned");
            match source_stream {
                StreamKind::Stdout => guard.stdout_closed = true,
                StreamKind::Stderr => guard.stderr_closed = true,
            }
            shared.condvar.notify_all();
        });
    }

    #[cfg(windows)]
    fn pipe_done_callback(&self, stream: StreamKind) -> Box<dyn FnOnce() + Send> {
        let handles = Arc::clone(&self.capture_pipe_handles);
        Box::new(move || {
            let mut guard = handles.lock().expect("capture pipe handles mutex poisoned");
            match stream {
                StreamKind::Stdout => guard.stdout = None,
                StreamKind::Stderr => guard.stderr = None,
            }
        })
    }

    #[cfg(not(windows))]
    fn pipe_done_callback(&self, _stream: StreamKind) -> Box<dyn FnOnce() + Send> {
        Box::new(|| {})
    }

    /// Cancel any pending blocking `read()` on the parent-side capture
    /// pipes so the reader threads' `read()` calls return
    /// `ERROR_OPERATION_ABORTED` immediately. Used by `kill_impl` to
    /// break the grandchild-orphan deadlock without waiting on
    /// `wait_for_capture_completion_with_deadline`'s safety-net.
    #[cfg(windows)]
    fn cancel_capture_io(&self) {
        crate::rp_rust_debug_scope!("running_process::NativeProcess::cancel_capture_io");
        cancel_capture_pipe_io(&self.capture_pipe_handles);
    }

    fn set_returncode(&self, code: i32) {
        self.shared.returncode.store(code as i64, Ordering::Release);
        self.shared.condvar.notify_all();
    }

    /// Bounded capture drain for the natural-exit and `close` paths
    /// (issue #590, cluster A). Waits at most `kill_drain_deadline` for the
    /// reader threads to flip the closed flags, force-setting them on
    /// timeout so `wait()`/`close()` return in bounded time instead of
    /// wedging in the previously-unbounded `wait_for_capture_completion`.
    /// Unlike `kill_impl` the reader is not cancelled up front — a
    /// short-lived grandchild's output is allowed to drain within the
    /// grace window — but if the window elapses with the pipe still held
    /// open the reader is cancelled to release the leaked thread (Windows).
    fn finish_capture_drain(&self) {
        let drained = self.wait_for_capture_completion_with_deadline_impl(kill_drain_deadline());
        #[cfg(windows)]
        if !drained {
            self.cancel_capture_io();
        }
        #[cfg(not(windows))]
        {
            let _ = drained;
        }
    }

    fn wait_for_capture_completion_impl(&self) {
        crate::rp_rust_debug_scope!("running_process::NativeProcess::wait_for_capture_completion");
        if !self.config.capture {
            return;
        }

        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");
        while !(guard.stdout_closed && guard.stderr_closed) {
            guard = self
                .shared
                .condvar
                .wait(guard)
                .expect("queue mutex poisoned");
        }
    }

    /// Like `wait_for_capture_completion_impl` but bounded by `deadline`.
    /// Returns `true` if the reader threads flipped both closed flags on
    /// their own, `false` if the deadline elapsed first. On timeout the
    /// closed flags are force-set (and waiters notified) so downstream
    /// pollers stop seeing `Timeout` and start seeing `Eof`. A reader
    /// thread that eventually unblocks after the OS releases the pipe
    /// will assign `closed = true` again, which is a harmless no-op.
    fn wait_for_capture_completion_with_deadline_impl(&self, deadline: Instant) -> bool {
        crate::rp_rust_debug_scope!(
            "running_process::NativeProcess::wait_for_capture_completion_with_deadline"
        );
        if !self.config.capture {
            return true;
        }
        finalize_capture_completion(&self.shared, deadline)
    }
}

/// Cancel any pending blocking `read()` on the parent-side capture pipes
/// so the reader threads' `read()` calls return `ERROR_OPERATION_ABORTED`
/// immediately. Shared by `kill_impl`, `poll`, and the natural-exit
/// waiter thread (issue #590) — anywhere the child is observed to exit
/// while a grandchild may still hold the pipe open.
#[cfg(windows)]
fn cancel_capture_pipe_io(handles: &Mutex<CapturePipeHandles>) {
    use winapi::shared::ntdef::HANDLE;
    use winapi::um::ioapiset::CancelIoEx;
    let guard = handles.lock().expect("capture pipe handles mutex poisoned");
    if let Some(h) = guard.stdout {
        // SAFETY: the slot is `Some` only while the owning reader thread
        // still holds the `ChildStdout`, so the HANDLE is valid for the
        // duration of this call. The reader is blocked in `lock()` on the
        // same mutex if it's racing us toward exit, so it cannot drop the
        // pipe and close the HANDLE until we return.
        unsafe {
            CancelIoEx(h as HANDLE, std::ptr::null_mut());
        }
    }
    if let Some(h) = guard.stderr {
        unsafe {
            CancelIoEx(h as HANDLE, std::ptr::null_mut());
        }
    }
}

/// Wait until both capture streams report closed or `deadline` elapses.
/// On deadline, force-set the closed flags (and notify all waiters) so
/// downstream pollers observe EOF instead of blocking forever. Returns
/// `true` if the reader threads flipped the flags on their own, `false`
/// if the deadline forced them. A reader thread that later unblocks and
/// re-sets `closed = true` is a harmless no-op.
fn finalize_capture_completion(shared: &SharedState, deadline: Instant) -> bool {
    let mut guard = shared.queues.lock().expect("queue mutex poisoned");
    while !(guard.stdout_closed && guard.stderr_closed) {
        let now = Instant::now();
        if now >= deadline {
            guard.stdout_closed = true;
            guard.stderr_closed = true;
            shared.condvar.notify_all();
            return false;
        }
        let (next_guard, result) = shared
            .condvar
            .wait_timeout(guard, deadline - now)
            .expect("queue mutex poisoned");
        guard = next_guard;
        if result.timed_out() && !(guard.stdout_closed && guard.stderr_closed) {
            guard.stdout_closed = true;
            guard.stderr_closed = true;
            shared.condvar.notify_all();
            return false;
        }
    }
    true
}

fn emit_lines(shared: &Arc<SharedState>, stream: StreamKind, lines: Vec<Vec<u8>>) {
    if lines.is_empty() {
        return;
    }
    let mut guard = shared.queues.lock().expect("queue mutex poisoned");
    for line in lines {
        let line_len = line.len();
        match stream {
            StreamKind::Stdout => {
                guard.stdout_history_bytes += line_len;
                guard.stdout_history.push_back(line.clone());
                guard.stdout_queue.push_back(line.clone());
            }
            StreamKind::Stderr => {
                guard.stderr_history_bytes += line_len;
                guard.stderr_history.push_back(line.clone());
                guard.stderr_queue.push_back(line.clone());
            }
        }
        let event = StreamEvent { stream, line };
        guard.combined_history_bytes += line_len;
        guard.combined_history.push_back(event.clone());
        guard.combined_queue.push_back(event);
    }
    shared.condvar.notify_all();
}

fn append_raw(shared: &Arc<SharedState>, stream: StreamKind, chunk: &[u8]) {
    if chunk.is_empty() {
        return;
    }
    let mut guard = shared.queues.lock().expect("queue mutex poisoned");
    match stream {
        StreamKind::Stdout => guard.stdout_raw.extend_from_slice(chunk),
        StreamKind::Stderr => guard.stderr_raw.extend_from_slice(chunk),
    }
}

/// Run a command to completion while concurrently draining stdout and stderr.
///
/// The helper forces capture on regardless of `config.capture`, returns raw
/// stdout/stderr bytes, and kills the child before returning
/// [`ProcessError::Timeout`] when `timeout` elapses.
pub fn run_command(
    mut config: ProcessConfig,
    timeout: Option<Duration>,
) -> Result<RunOutput, ProcessError> {
    config.capture = true;
    let process = NativeProcess::new(config);
    process.start()?;

    let exit_code = match process.wait(timeout) {
        Ok(code) => code,
        Err(ProcessError::Timeout) => {
            match process.kill() {
                Ok(()) | Err(ProcessError::NotRunning) => {}
                Err(error) => return Err(error),
            }
            return Err(ProcessError::Timeout);
        }
        Err(error) => return Err(error),
    };

    Ok(RunOutput {
        stdout: process.captured_stdout_raw(),
        stderr: process.captured_stderr_raw(),
        exit_code,
    })
}

pub(crate) fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        let mut cmd = Command::new("cmd");
        cmd.raw_arg("/D /S /C \"");
        cmd.raw_arg(command);
        cmd.raw_arg("\"");
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.arg("-lc").arg(command);
        cmd
    }
}

#[cfg(test)]
mod tests;
