use std::collections::VecDeque;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub mod console_detect;
pub mod containment;
mod helpers;
#[cfg(feature = "originator-scan")]
pub mod originator;
// Wave 3 of #165: proto module absorbed from `running-process-proto`.
// The submodule name `daemon` matches the protobuf package
// `running_process.daemon.v1`. Will be feature-gated behind
// `feature = "client"` in Wave 4 along with the rest of IPC.
pub mod proto {
    pub mod daemon {
        include!(concat!(env!("OUT_DIR"), "/running_process.daemon.v1.rs"));
    }
}
pub mod pty;
mod public_symbols;
mod rust_debug;
pub mod spawn;
mod types;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

pub use console_detect::{monitor_console_windows, ConsoleWindowInfo};
pub use containment::{ContainedProcessGroup, ORIGINATOR_ENV_VAR};
#[cfg(feature = "originator-scan")]
pub use originator::{find_processes_by_originator, OriginatorProcessInfo};
pub use rust_debug::{render_rust_debug_traces, RustDebugScopeGuard};
pub use spawn::{
    spawn, spawn_daemon, spawn_daemon_with_clear_env, DaemonChild, SpawnStdio, SpawnedChild,
    StdioSource,
};
pub use types::{
    CommandSpec, ProcessConfig, ProcessError, ReadStatus, StderrMode, StdinMode, StreamEvent,
    StreamKind,
};

pub(crate) use helpers::{exit_code, feed_chunk, kill_drain_deadline, log_spawned_child_pid};
#[cfg(unix)]
pub use unix::{unix_set_priority, unix_signal_process, unix_signal_process_group, UnixSignal};
#[cfg(windows)]
pub(crate) use windows::{
    assign_child_to_windows_kill_on_close_job_impl, windows_priority_flags, CapturePipeHandles,
    WindowsJobHandle,
};

#[macro_export]
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
}

struct ChildState {
    child: Child,
    #[cfg(windows)]
    _job: WindowsJobHandle,
}

impl SharedState {
    fn new(capture: bool) -> Self {
        let queues = QueueState {
            stdout_closed: !capture,
            stderr_closed: !capture,
            ..QueueState::default()
        };
        Self {
            queues: Mutex::new(queues),
            condvar: Condvar::new(),
            returncode: AtomicI64::new(RETURNCODE_NOT_SET),
        }
    }
}

pub struct NativeProcess {
    config: ProcessConfig,
    child: Arc<Mutex<Option<ChildState>>>,
    shared: Arc<SharedState>,
    #[cfg(windows)]
    capture_pipe_handles: Arc<Mutex<CapturePipeHandles>>,
}

impl NativeProcess {
    pub fn new(config: ProcessConfig) -> Self {
        Self {
            shared: Arc::new(SharedState::new(config.capture)),
            child: Arc::new(Mutex::new(None)),
            config,
            #[cfg(windows)]
            capture_pipe_handles: Arc::new(Mutex::new(CapturePipeHandles::default())),
        }
    }

    // Preserve a stable Rust frame here in release user dumps.
    #[inline(never)]
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
        #[cfg(windows)]
        let job = public_symbols::rp_assign_child_to_windows_kill_on_close_job_public(&child)
            .map_err(ProcessError::Spawn)?;
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
        thread::spawn(move || loop {
            if shared.returncode.load(Ordering::Acquire) != RETURNCODE_NOT_SET {
                return;
            }
            {
                let mut guard = child.lock().expect("child mutex poisoned");
                if let Some(child_state) = guard.as_mut() {
                    match child_state.child.try_wait() {
                        Ok(Some(status)) => {
                            let code = exit_code(status);
                            shared.returncode.store(code as i64, Ordering::Release);
                            shared.condvar.notify_all();
                            return;
                        }
                        Ok(None) => {}
                        Err(_) => return,
                    }
                } else {
                    return;
                }
            }
            thread::sleep(Duration::from_millis(10));
        });
    }

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

    pub fn poll(&self) -> Result<Option<i32>, ProcessError> {
        // Fast path: check atomic set by background waiter thread.
        if let Some(code) = self.returncode() {
            return Ok(Some(code));
        }
        let mut guard = self.child.lock().expect("child mutex poisoned");
        let Some(child_state) = guard.as_mut() else {
            return Ok(self.returncode());
        };
        let child = &mut child_state.child;
        let status = child.try_wait().map_err(ProcessError::Io)?;
        if let Some(status) = status {
            let code = exit_code(status);
            self.set_returncode(code);
            return Ok(Some(code));
        }
        Ok(None)
    }

    // Preserve a stable Rust frame here in release user dumps.
    #[inline(never)]
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
            public_symbols::rp_native_process_wait_for_capture_completion_public(self);
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
                public_symbols::rp_native_process_wait_for_capture_completion_public(self);
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
    pub fn kill(&self) -> Result<(), ProcessError> {
        public_symbols::rp_native_process_kill_public(self)
    }

    fn kill_impl(&self) -> Result<(), ProcessError> {
        crate::rp_rust_debug_scope!("running_process::NativeProcess::kill");
        {
            let mut guard = self.child.lock().expect("child mutex poisoned");
            let child = &mut guard.as_mut().ok_or(ProcessError::NotRunning)?.child;
            child.kill().map_err(ProcessError::Io)?;
            let status = child.wait().map_err(ProcessError::Io)?;
            self.set_returncode(exit_code(status));
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
            public_symbols::rp_native_process_wait_for_capture_completion_public(self);
        }
        Ok(())
    }

    pub fn pid(&self) -> Option<u32> {
        self.child
            .lock()
            .expect("child mutex poisoned")
            .as_ref()
            .map(|state| state.child.id())
    }

    pub fn returncode(&self) -> Option<i32> {
        let v = self.shared.returncode.load(Ordering::Acquire);
        if v == RETURNCODE_NOT_SET {
            None
        } else {
            Some(v as i32)
        }
    }

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

    pub fn has_pending_combined(&self) -> bool {
        let guard = self.shared.queues.lock().expect("queue mutex poisoned");
        !guard.combined_queue.is_empty()
    }

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

    pub fn drain_combined(&self) -> Vec<StreamEvent> {
        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");
        guard.combined_queue.drain(..).collect()
    }

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

    pub fn captured_combined_bytes(&self) -> usize {
        self.shared
            .queues
            .lock()
            .expect("queue mutex poisoned")
            .combined_history_bytes
    }

    pub fn clear_captured_stream(&self, stream: StreamKind) -> usize {
        if stream == StreamKind::Stderr && self.config.stderr_mode == StderrMode::Stdout {
            return 0;
        }
        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");
        match stream {
            StreamKind::Stdout => {
                let released = guard.stdout_history_bytes;
                guard.stdout_history.clear();
                guard.stdout_history_bytes = 0;
                released
            }
            StreamKind::Stderr => {
                let released = guard.stderr_history_bytes;
                guard.stderr_history.clear();
                guard.stderr_history_bytes = 0;
                released
            }
        }
    }

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

            // CREATE_NEW_PROCESS_GROUP makes GenerateConsoleCtrlEvent
            // with CTRL_BREAK_EVENT route to this child's group
            // (rather than the daemon's group) — required for the
            // pipe-session soft-signal path on Windows (#130 M4).
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            let extra = if self.config.create_process_group {
                CREATE_NEW_PROCESS_GROUP
            } else {
                0
            };
            let flags = self.config.creationflags.unwrap_or(0)
                | extra
                | windows_priority_flags(self.config.nice);
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
            let mut guard = handles
                .lock()
                .expect("capture pipe handles mutex poisoned");
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
        use winapi::shared::ntdef::HANDLE;
        use winapi::um::ioapiset::CancelIoEx;
        let guard = self
            .capture_pipe_handles
            .lock()
            .expect("capture pipe handles mutex poisoned");
        if let Some(h) = guard.stdout {
            // SAFETY: the slot is `Some` only while the owning reader
            // thread still holds the `ChildStdout`, so the HANDLE is
            // valid for the duration of this call. The reader is
            // blocked in `lock()` on the same mutex if it's racing us
            // toward exit, so it cannot drop the pipe and close the
            // HANDLE until we return.
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

    fn set_returncode(&self, code: i32) {
        self.shared.returncode.store(code as i64, Ordering::Release);
        self.shared.condvar.notify_all();
    }

    fn wait_for_capture_completion_impl(&self) {
        crate::rp_rust_debug_scope!(
            "running_process::NativeProcess::wait_for_capture_completion"
        );
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

        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");
        while !(guard.stdout_closed && guard.stderr_closed) {
            let now = Instant::now();
            if now >= deadline {
                guard.stdout_closed = true;
                guard.stderr_closed = true;
                self.shared.condvar.notify_all();
                return false;
            }
            let (next_guard, result) = self
                .shared
                .condvar
                .wait_timeout(guard, deadline - now)
                .expect("queue mutex poisoned");
            guard = next_guard;
            if result.timed_out() && !(guard.stdout_closed && guard.stderr_closed) {
                guard.stdout_closed = true;
                guard.stderr_closed = true;
                self.shared.condvar.notify_all();
                return false;
            }
        }
        true
    }
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
