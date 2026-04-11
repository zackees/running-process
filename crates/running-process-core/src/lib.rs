use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

pub mod containment;
#[cfg(feature = "originator-scan")]
pub mod originator;
mod public_symbols;
mod rust_debug;

pub use containment::{ContainedChild, ContainedProcessGroup, Containment, ORIGINATOR_ENV_VAR};
#[cfg(feature = "originator-scan")]
pub use originator::{find_processes_by_originator, OriginatorProcessInfo};
pub use rust_debug::{render_rust_debug_traces, RustDebugScopeGuard};

#[macro_export]
macro_rules! rp_rust_debug_scope {
    ($label:expr) => {
        let _running_process_rust_debug_scope =
            $crate::RustDebugScopeGuard::enter($label, file!(), line!());
    };
}

const CHILD_PID_LOG_PATH_ENV: &str = "RUNNING_PROCESS_CHILD_PID_LOG_PATH";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

impl StreamKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEvent {
    pub stream: StreamKind,
    pub line: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadStatus<T> {
    Line(T),
    Timeout,
    Eof,
}

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("process already started")]
    AlreadyStarted,
    #[error("process is not running")]
    NotRunning,
    #[error("process stdin is not available")]
    StdinUnavailable,
    #[error("failed to spawn process: {0}")]
    Spawn(std::io::Error),
    #[error("failed to read process output: {0}")]
    Io(std::io::Error),
    #[error("process timed out")]
    Timeout,
}

#[derive(Debug, Clone)]
pub enum CommandSpec {
    Shell(String),
    Argv(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdinMode {
    Inherit,
    Piped,
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StderrMode {
    Stdout,
    Pipe,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixSignal {
    Interrupt,
    Terminate,
    Kill,
}

#[derive(Debug, Clone)]
pub struct ProcessConfig {
    pub command: CommandSpec,
    pub cwd: Option<PathBuf>,
    pub env: Option<Vec<(String, String)>>,
    pub capture: bool,
    pub stderr_mode: StderrMode,
    pub creationflags: Option<u32>,
    pub create_process_group: bool,
    pub stdin_mode: StdinMode,
    pub nice: Option<i32>,
    /// Optional containment policy. `None` preserves existing behaviour.
    /// `Some(Contained)` sets `PR_SET_PDEATHSIG(SIGKILL)` on Linux and uses
    /// the existing Job Object on Windows. `Some(Detached)` creates a new
    /// session (`setsid`) on Unix so the child survives the parent.
    pub containment: Option<Containment>,
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

#[cfg(windows)]
struct WindowsJobHandle(usize);

#[cfg(windows)]
impl Drop for WindowsJobHandle {
    fn drop(&mut self) {
        unsafe {
            winapi::um::handleapi::CloseHandle(self.0 as winapi::shared::ntdef::HANDLE);
        }
    }
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
}

impl NativeProcess {
    pub fn new(config: ProcessConfig) -> Self {
        Self {
            shared: Arc::new(SharedState::new(config.capture)),
            child: Arc::new(Mutex::new(None)),
            config,
        }
    }

    // Preserve a stable Rust frame here in release user dumps.
    #[inline(never)]
    pub fn start(&self) -> Result<(), ProcessError> {
        public_symbols::rp_native_process_start_public(self)
    }

    fn start_impl(&self) -> Result<(), ProcessError> {
        crate::rp_rust_debug_scope!("running_process_core::NativeProcess::start");
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
            self.spawn_reader(stdout, StreamKind::Stdout, StreamKind::Stdout);
            self.spawn_reader(
                stderr,
                StreamKind::Stderr,
                match self.config.stderr_mode {
                    StderrMode::Stdout => StreamKind::Stdout,
                    StderrMode::Pipe => StreamKind::Stderr,
                },
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
            thread::sleep(Duration::from_millis(1));
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
        crate::rp_rust_debug_scope!("running_process_core::NativeProcess::wait");
        if self.child.lock().expect("child mutex poisoned").is_none() {
            return self.returncode().ok_or(ProcessError::NotRunning);
        }
        let start = Instant::now();
        loop {
            if let Some(code) = self.poll()? {
                public_symbols::rp_native_process_wait_for_capture_completion_public(self);
                return Ok(code);
            }
            if timeout.is_some_and(|limit| start.elapsed() >= limit) {
                return Err(ProcessError::Timeout);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    // Preserve a stable Rust frame here in release user dumps.
    #[inline(never)]
    pub fn kill(&self) -> Result<(), ProcessError> {
        public_symbols::rp_native_process_kill_public(self)
    }

    fn kill_impl(&self) -> Result<(), ProcessError> {
        crate::rp_rust_debug_scope!("running_process_core::NativeProcess::kill");
        let mut guard = self.child.lock().expect("child mutex poisoned");
        let child = &mut guard.as_mut().ok_or(ProcessError::NotRunning)?.child;
        child.kill().map_err(ProcessError::Io)?;
        let status = child.wait().map_err(ProcessError::Io)?;
        self.set_returncode(exit_code(status));
        Ok(())
    }

    pub fn terminate(&self) -> Result<(), ProcessError> {
        self.kill()
    }

    // Preserve a stable Rust frame here in release user dumps.
    #[inline(never)]
    pub fn close(&self) -> Result<(), ProcessError> {
        public_symbols::rp_native_process_close_public(self)
    }

    fn close_impl(&self) -> Result<(), ProcessError> {
        crate::rp_rust_debug_scope!("running_process_core::NativeProcess::close");
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
        crate::rp_rust_debug_scope!("running_process_core::NativeProcess::read_combined");
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

            let flags =
                self.config.creationflags.unwrap_or(0) | windows_priority_flags(self.config.nice);
            if flags != 0 {
                command.creation_flags(flags);
            }
        }
        #[cfg(unix)]
        {
            let create_process_group = self.config.create_process_group;
            let nice = self.config.nice;
            let containment = self.config.containment;

            let needs_pre_exec = create_process_group || nice.is_some() || containment.is_some();

            if needs_pre_exec {
                use std::os::unix::process::CommandExt;

                unsafe {
                    command.pre_exec(move || {
                        match containment {
                            Some(Containment::Contained) => {
                                // Place child into its own process group.
                                if libc::setpgid(0, 0) == -1 {
                                    return Err(std::io::Error::last_os_error());
                                }
                                // Linux: ask the kernel to SIGKILL us when the
                                // parent thread dies.
                                // CAVEAT: PR_SET_PDEATHSIG is per-thread, not
                                // per-process. If the spawning thread exits
                                // before the process, the child is killed early.
                                #[cfg(target_os = "linux")]
                                {
                                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                                        return Err(std::io::Error::last_os_error());
                                    }
                                    if libc::getppid() == 1 {
                                        libc::_exit(1);
                                    }
                                }
                            }
                            Some(Containment::Detached) => {
                                // Create a new session so the child is fully
                                // independent of the parent.
                                if libc::setsid() == -1 {
                                    return Err(std::io::Error::last_os_error());
                                }
                            }
                            None => {
                                if create_process_group && libc::setpgid(0, 0) == -1 {
                                    return Err(std::io::Error::last_os_error());
                                }
                            }
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

    fn spawn_reader<R>(&self, pipe: R, source_stream: StreamKind, visible_stream: StreamKind)
    where
        R: Read + Send + 'static,
    {
        let shared = Arc::clone(&self.shared);
        thread::spawn(move || {
            let mut reader = pipe;
            let mut chunk = [0_u8; 4096];
            let mut pending = Vec::new();

            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => feed_chunk(&shared, visible_stream, &mut pending, &chunk[..n]),
                    Err(_) => break,
                }
            }

            if !pending.is_empty() {
                emit_line(&shared, visible_stream, std::mem::take(&mut pending));
            }

            let mut guard = shared.queues.lock().expect("queue mutex poisoned");
            match source_stream {
                StreamKind::Stdout => guard.stdout_closed = true,
                StreamKind::Stderr => guard.stderr_closed = true,
            }
            shared.condvar.notify_all();
        });
    }

    fn set_returncode(&self, code: i32) {
        self.shared.returncode.store(code as i64, Ordering::Release);
        self.shared.condvar.notify_all();
    }

    fn wait_for_capture_completion_impl(&self) {
        crate::rp_rust_debug_scope!(
            "running_process_core::NativeProcess::wait_for_capture_completion"
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
}

#[cfg(unix)]
pub fn unix_set_priority(pid: u32, nice: i32) -> Result<(), std::io::Error> {
    let result = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid, nice) };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
pub fn unix_signal_process(pid: u32, signal: UnixSignal) -> Result<(), std::io::Error> {
    let result = unsafe { libc::kill(pid as i32, unix_signal_raw(signal)) };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
pub fn unix_signal_process_group(pid: i32, signal: UnixSignal) -> Result<(), std::io::Error> {
    let result = unsafe { libc::killpg(pid, unix_signal_raw(signal)) };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn log_spawned_child_pid(pid: u32) -> Result<(), std::io::Error> {
    let Some(path) = std::env::var_os(CHILD_PID_LOG_PATH_ENV) else {
        return Ok(());
    };

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(format!("{pid}\n").as_bytes())?;
    file.flush()?;
    Ok(())
}

#[cfg(windows)]
fn assign_child_to_windows_kill_on_close_job_impl(
    child: &Child,
) -> Result<WindowsJobHandle, std::io::Error> {
    crate::rp_rust_debug_scope!("running_process_core::assign_child_to_windows_kill_on_close_job");
    use std::mem::zeroed;
    use std::os::windows::io::AsRawHandle;

    use winapi::shared::minwindef::FALSE;
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::jobapi2::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    };
    use winapi::um::winnt::{
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    let handle = child.as_raw_handle();
    let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
    if job.is_null() || job == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }

    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == FALSE {
        let err = std::io::Error::last_os_error();
        unsafe { CloseHandle(job) };
        return Err(err);
    }

    let ok = unsafe { AssignProcessToJobObject(job, handle.cast()) };
    if ok == FALSE {
        let err = std::io::Error::last_os_error();
        unsafe { CloseHandle(job) };
        return Err(err);
    }

    Ok(WindowsJobHandle(job as usize))
}

fn feed_chunk(shared: &Arc<SharedState>, stream: StreamKind, pending: &mut Vec<u8>, chunk: &[u8]) {
    let mut start = 0;
    let mut index = 0;

    while index < chunk.len() {
        if chunk[index] == b'\n' {
            let end = if index > start && chunk[index - 1] == b'\r' {
                index - 1
            } else {
                index
            };
            pending.extend_from_slice(&chunk[start..end]);
            if !pending.is_empty() {
                emit_line(shared, stream, std::mem::take(pending));
            }
            start = index + 1;
        }
        index += 1;
    }

    pending.extend_from_slice(&chunk[start..]);
}

fn emit_line(shared: &Arc<SharedState>, stream: StreamKind, line: Vec<u8>) {
    let event = StreamEvent { stream, line };
    let mut guard = shared.queues.lock().expect("queue mutex poisoned");
    match event.stream {
        StreamKind::Stdout => {
            guard.stdout_history_bytes += event.line.len();
            guard.stdout_history.push_back(event.line.clone());
            guard.stdout_queue.push_back(event.line.clone());
        }
        StreamKind::Stderr => {
            guard.stderr_history_bytes += event.line.len();
            guard.stderr_history.push_back(event.line.clone());
            guard.stderr_queue.push_back(event.line.clone());
        }
    }
    guard.combined_history_bytes += event.line.len();
    guard.combined_history.push_back(event.clone());
    guard.combined_queue.push_back(event);
    shared.condvar.notify_all();
}

fn shell_command(command: &str) -> Command {
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

fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(1))
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(1)
    }
}

#[cfg(unix)]
fn unix_signal_raw(signal: UnixSignal) -> i32 {
    match signal {
        UnixSignal::Interrupt => libc::SIGINT,
        UnixSignal::Terminate => libc::SIGTERM,
        UnixSignal::Kill => libc::SIGKILL,
    }
}

#[cfg(windows)]
fn windows_priority_flags(nice: Option<i32>) -> u32 {
    const IDLE_PRIORITY_CLASS: u32 = 0x0000_0040;
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
    const ABOVE_NORMAL_PRIORITY_CLASS: u32 = 0x0000_8000;
    const HIGH_PRIORITY_CLASS: u32 = 0x0000_0080;

    match nice {
        Some(value) if value >= 15 => IDLE_PRIORITY_CLASS,
        Some(value) if value >= 1 => BELOW_NORMAL_PRIORITY_CLASS,
        Some(value) if value <= -15 => HIGH_PRIORITY_CLASS,
        Some(value) if value <= -1 => ABOVE_NORMAL_PRIORITY_CLASS,
        _ => 0,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    // ── StreamKind tests ──

    #[test]
    fn stream_kind_as_str_stdout() {
        assert_eq!(StreamKind::Stdout.as_str(), "stdout");
    }

    #[test]
    fn stream_kind_as_str_stderr() {
        assert_eq!(StreamKind::Stderr.as_str(), "stderr");
    }

    #[test]
    fn stream_kind_equality() {
        assert_eq!(StreamKind::Stdout, StreamKind::Stdout);
        assert_ne!(StreamKind::Stdout, StreamKind::Stderr);
    }

    // ── StreamEvent tests ──

    #[test]
    fn stream_event_clone() {
        let event = StreamEvent {
            stream: StreamKind::Stdout,
            line: b"hello".to_vec(),
        };
        let cloned = event.clone();
        assert_eq!(event, cloned);
    }

    // ── ReadStatus tests ──

    #[test]
    fn read_status_line_variant() {
        let status: ReadStatus<Vec<u8>> = ReadStatus::Line(b"data".to_vec());
        assert!(matches!(status, ReadStatus::Line(ref v) if v == b"data"));
    }

    #[test]
    fn read_status_timeout_variant() {
        let status: ReadStatus<Vec<u8>> = ReadStatus::Timeout;
        assert!(matches!(status, ReadStatus::Timeout));
    }

    #[test]
    fn read_status_eof_variant() {
        let status: ReadStatus<Vec<u8>> = ReadStatus::Eof;
        assert!(matches!(status, ReadStatus::Eof));
    }

    // ── ProcessError tests ──

    #[test]
    fn process_error_display_already_started() {
        assert_eq!(
            ProcessError::AlreadyStarted.to_string(),
            "process already started"
        );
    }

    #[test]
    fn process_error_display_not_running() {
        assert_eq!(
            ProcessError::NotRunning.to_string(),
            "process is not running"
        );
    }

    #[test]
    fn process_error_display_stdin_unavailable() {
        assert_eq!(
            ProcessError::StdinUnavailable.to_string(),
            "process stdin is not available"
        );
    }

    #[test]
    fn process_error_display_timeout() {
        assert_eq!(ProcessError::Timeout.to_string(), "process timed out");
    }

    #[test]
    fn process_error_display_spawn() {
        let err = ProcessError::Spawn(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not found",
        ));
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn process_error_display_io() {
        let err = ProcessError::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "broken",
        ));
        assert!(err.to_string().contains("broken"));
    }

    // ── CommandSpec tests ──

    #[test]
    fn command_spec_shell_variant() {
        let spec = CommandSpec::Shell("echo hello".to_string());
        assert!(matches!(spec, CommandSpec::Shell(ref s) if s == "echo hello"));
    }

    #[test]
    fn command_spec_argv_variant() {
        let spec = CommandSpec::Argv(vec!["echo".to_string(), "hello".to_string()]);
        assert!(matches!(spec, CommandSpec::Argv(ref v) if v.len() == 2));
    }

    // ── StdinMode / StderrMode tests ──

    #[test]
    fn stdin_mode_equality() {
        assert_eq!(StdinMode::Inherit, StdinMode::Inherit);
        assert_ne!(StdinMode::Piped, StdinMode::Null);
    }

    #[test]
    fn stderr_mode_equality() {
        assert_eq!(StderrMode::Stdout, StderrMode::Stdout);
        assert_ne!(StderrMode::Stdout, StderrMode::Pipe);
    }

    // ── SharedState tests ──

    #[test]
    fn shared_state_new_with_capture() {
        let state = SharedState::new(true);
        let queues = state.queues.lock().unwrap();
        assert!(!queues.stdout_closed);
        assert!(!queues.stderr_closed);
        assert!(queues.stdout_queue.is_empty());
        assert!(queues.stderr_queue.is_empty());
    }

    #[test]
    fn shared_state_new_without_capture() {
        let state = SharedState::new(false);
        let queues = state.queues.lock().unwrap();
        assert!(queues.stdout_closed);
        assert!(queues.stderr_closed);
    }

    #[test]
    fn shared_state_returncode_initially_not_set() {
        let state = SharedState::new(true);
        let code = state.returncode.load(Ordering::Acquire);
        assert_eq!(code, RETURNCODE_NOT_SET);
    }

    // ── feed_chunk tests ──

    #[test]
    fn feed_chunk_single_line_with_newline() {
        let shared = Arc::new(SharedState::new(true));
        let mut pending = Vec::new();
        feed_chunk(&shared, StreamKind::Stdout, &mut pending, b"hello\n");
        let queues = shared.queues.lock().unwrap();
        assert_eq!(queues.stdout_queue.len(), 1);
        assert_eq!(queues.stdout_queue[0], b"hello");
        assert!(pending.is_empty());
    }

    #[test]
    fn feed_chunk_crlf_stripping() {
        let shared = Arc::new(SharedState::new(true));
        let mut pending = Vec::new();
        feed_chunk(&shared, StreamKind::Stdout, &mut pending, b"hello\r\n");
        let queues = shared.queues.lock().unwrap();
        assert_eq!(queues.stdout_queue.len(), 1);
        assert_eq!(queues.stdout_queue[0], b"hello");
    }

    #[test]
    fn feed_chunk_multiple_lines() {
        let shared = Arc::new(SharedState::new(true));
        let mut pending = Vec::new();
        feed_chunk(&shared, StreamKind::Stdout, &mut pending, b"a\nb\nc\n");
        let queues = shared.queues.lock().unwrap();
        assert_eq!(queues.stdout_queue.len(), 3);
        assert_eq!(queues.stdout_queue[0], b"a");
        assert_eq!(queues.stdout_queue[1], b"b");
        assert_eq!(queues.stdout_queue[2], b"c");
    }

    #[test]
    fn feed_chunk_no_newline_stays_pending() {
        let shared = Arc::new(SharedState::new(true));
        let mut pending = Vec::new();
        feed_chunk(&shared, StreamKind::Stdout, &mut pending, b"partial");
        let queues = shared.queues.lock().unwrap();
        assert!(queues.stdout_queue.is_empty());
        assert_eq!(pending, b"partial");
    }

    #[test]
    fn feed_chunk_accumulates_pending() {
        let shared = Arc::new(SharedState::new(true));
        let mut pending = Vec::new();
        feed_chunk(&shared, StreamKind::Stdout, &mut pending, b"hel");
        feed_chunk(&shared, StreamKind::Stdout, &mut pending, b"lo\n");
        let queues = shared.queues.lock().unwrap();
        assert_eq!(queues.stdout_queue.len(), 1);
        assert_eq!(queues.stdout_queue[0], b"hello");
        assert!(pending.is_empty());
    }

    #[test]
    fn feed_chunk_empty_line_not_emitted() {
        let shared = Arc::new(SharedState::new(true));
        let mut pending = Vec::new();
        feed_chunk(&shared, StreamKind::Stdout, &mut pending, b"\n");
        let queues = shared.queues.lock().unwrap();
        assert!(queues.stdout_queue.is_empty());
    }

    #[test]
    fn feed_chunk_stderr_goes_to_stderr_queue() {
        let shared = Arc::new(SharedState::new(true));
        let mut pending = Vec::new();
        feed_chunk(&shared, StreamKind::Stderr, &mut pending, b"error\n");
        let queues = shared.queues.lock().unwrap();
        assert!(queues.stdout_queue.is_empty());
        assert_eq!(queues.stderr_queue.len(), 1);
        assert_eq!(queues.stderr_queue[0], b"error");
    }

    // ── emit_line tests ──

    #[test]
    fn emit_line_updates_all_queues_and_history() {
        let shared = Arc::new(SharedState::new(true));
        emit_line(&shared, StreamKind::Stdout, b"test".to_vec());
        let queues = shared.queues.lock().unwrap();
        assert_eq!(queues.stdout_queue.len(), 1);
        assert_eq!(queues.stdout_history.len(), 1);
        assert_eq!(queues.stdout_history_bytes, 4);
        assert_eq!(queues.combined_queue.len(), 1);
        assert_eq!(queues.combined_history.len(), 1);
        assert_eq!(queues.combined_history_bytes, 4);
    }

    #[test]
    fn emit_line_stderr_updates_stderr_queues() {
        let shared = Arc::new(SharedState::new(true));
        emit_line(&shared, StreamKind::Stderr, b"err".to_vec());
        let queues = shared.queues.lock().unwrap();
        assert_eq!(queues.stderr_queue.len(), 1);
        assert_eq!(queues.stderr_history.len(), 1);
        assert_eq!(queues.stderr_history_bytes, 3);
        assert_eq!(queues.combined_queue.len(), 1);
        assert_eq!(queues.combined_history_bytes, 3);
    }

    // ── NativeProcess unit tests (no process spawn) ──

    #[test]
    fn native_process_returncode_none_before_start() {
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec!["echo".into()]),
            cwd: None,
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            containment: None,
        });
        assert!(process.returncode().is_none());
    }

    #[test]
    fn native_process_pid_none_before_start() {
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec!["echo".into()]),
            cwd: None,
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            containment: None,
        });
        assert!(process.pid().is_none());
    }

    #[test]
    fn native_process_has_pending_false_when_no_capture() {
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec!["echo".into()]),
            cwd: None,
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            containment: None,
        });
        assert!(!process.has_pending_stream(StreamKind::Stdout));
        assert!(!process.has_pending_combined());
    }

    #[test]
    fn native_process_drain_empty_when_no_capture() {
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec!["echo".into()]),
            cwd: None,
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            containment: None,
        });
        assert!(process.drain_stream(StreamKind::Stdout).is_empty());
        assert!(process.drain_combined().is_empty());
    }

    #[test]
    fn native_process_stderr_not_pending_when_merged() {
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec!["echo".into()]),
            cwd: None,
            env: None,
            capture: true,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            containment: None,
        });
        assert!(!process.has_pending_stream(StreamKind::Stderr));
    }

    #[test]
    fn native_process_drain_stderr_empty_when_merged() {
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec!["echo".into()]),
            cwd: None,
            env: None,
            capture: true,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            containment: None,
        });
        assert!(process.drain_stream(StreamKind::Stderr).is_empty());
    }

    #[test]
    fn native_process_captured_stderr_empty_when_merged() {
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec!["echo".into()]),
            cwd: None,
            env: None,
            capture: true,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            containment: None,
        });
        assert!(process.captured_stderr().is_empty());
    }

    #[test]
    fn native_process_captured_stream_bytes_zero_when_merged_stderr() {
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec!["echo".into()]),
            cwd: None,
            env: None,
            capture: true,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            containment: None,
        });
        assert_eq!(process.captured_stream_bytes(StreamKind::Stderr), 0);
    }

    #[test]
    fn native_process_clear_captured_stderr_zero_when_merged() {
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec!["echo".into()]),
            cwd: None,
            env: None,
            capture: true,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            containment: None,
        });
        assert_eq!(process.clear_captured_stream(StreamKind::Stderr), 0);
    }

    #[test]
    fn native_process_read_stream_eof_when_stderr_merged() {
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec!["echo".into()]),
            cwd: None,
            env: None,
            capture: true,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            containment: None,
        });
        assert_eq!(
            process.read_stream(StreamKind::Stderr, Some(Duration::from_millis(10))),
            ReadStatus::Eof
        );
    }

    // ── log_spawned_child_pid ──

    #[test]
    fn log_spawned_child_pid_noop_without_env() {
        std::env::remove_var("RUNNING_PROCESS_CHILD_PID_LOG_PATH");
        assert!(log_spawned_child_pid(12345).is_ok());
    }

    // ── shell_command ──

    #[test]
    fn shell_command_creates_command() {
        let cmd = shell_command("echo test");
        let _ = format!("{:?}", cmd);
    }

    // ── exit_code ──

    #[test]
    fn exit_code_from_success() {
        let output = std::process::Command::new("python")
            .args(["-c", "pass"])
            .output()
            .unwrap();
        assert_eq!(exit_code(output.status), 0);
    }

    #[test]
    fn exit_code_from_nonzero() {
        let output = std::process::Command::new("python")
            .args(["-c", "import sys; sys.exit(42)"])
            .output()
            .unwrap();
        assert_eq!(exit_code(output.status), 42);
    }

    // ── windows_priority_flags ──

    #[cfg(windows)]
    mod windows_tests {
        use super::*;

        const IDLE_PRIORITY_CLASS: u32 = 0x0000_0040;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        const ABOVE_NORMAL_PRIORITY_CLASS: u32 = 0x0000_8000;
        const HIGH_PRIORITY_CLASS: u32 = 0x0000_0080;

        #[test]
        fn priority_flags_none() {
            assert_eq!(windows_priority_flags(None), 0);
        }

        #[test]
        fn priority_flags_zero() {
            assert_eq!(windows_priority_flags(Some(0)), 0);
        }

        #[test]
        fn priority_flags_high_nice_idle() {
            assert_eq!(windows_priority_flags(Some(15)), IDLE_PRIORITY_CLASS);
            assert_eq!(windows_priority_flags(Some(20)), IDLE_PRIORITY_CLASS);
        }

        #[test]
        fn priority_flags_low_positive_below_normal() {
            assert_eq!(windows_priority_flags(Some(1)), BELOW_NORMAL_PRIORITY_CLASS);
            assert_eq!(
                windows_priority_flags(Some(14)),
                BELOW_NORMAL_PRIORITY_CLASS
            );
        }

        #[test]
        fn priority_flags_negative_above_normal() {
            assert_eq!(
                windows_priority_flags(Some(-1)),
                ABOVE_NORMAL_PRIORITY_CLASS
            );
            assert_eq!(
                windows_priority_flags(Some(-14)),
                ABOVE_NORMAL_PRIORITY_CLASS
            );
        }

        #[test]
        fn priority_flags_very_negative_high() {
            assert_eq!(windows_priority_flags(Some(-15)), HIGH_PRIORITY_CLASS);
            assert_eq!(windows_priority_flags(Some(-20)), HIGH_PRIORITY_CLASS);
        }
    }

    // ── ProcessConfig ──

    #[test]
    fn process_config_clone() {
        let config = ProcessConfig {
            command: CommandSpec::Shell("echo".to_string()),
            cwd: Some("/tmp".into()),
            env: Some(vec![("KEY".to_string(), "VAL".to_string())]),
            capture: true,
            stderr_mode: StderrMode::Pipe,
            creationflags: Some(0x10),
            create_process_group: true,
            stdin_mode: StdinMode::Piped,
            nice: Some(5),
            containment: None,
        };
        let cloned = config.clone();
        assert!(cloned.capture);
        assert_eq!(cloned.nice, Some(5));
    }

    // ── render_rust_debug_traces ──

    #[test]
    fn render_rust_debug_traces_returns_string() {
        let result = render_rust_debug_traces();
        let _ = result.len();
    }

    // ── RustDebugScopeGuard ──

    #[test]
    fn rust_debug_scope_guard_enters_and_drops() {
        let _guard = RustDebugScopeGuard::enter("test_scope", file!(), line!());
        let traces = render_rust_debug_traces();
        assert!(traces.contains("test_scope"));
        drop(_guard);
    }

    // ── Unix signal tests ──

    #[cfg(unix)]
    mod unix_tests {
        use super::*;

        #[test]
        fn unix_signal_raw_values() {
            assert_eq!(unix_signal_raw(UnixSignal::Interrupt), libc::SIGINT);
            assert_eq!(unix_signal_raw(UnixSignal::Terminate), libc::SIGTERM);
            assert_eq!(unix_signal_raw(UnixSignal::Kill), libc::SIGKILL);
        }

        #[test]
        fn unix_signal_enum_equality() {
            assert_eq!(UnixSignal::Interrupt, UnixSignal::Interrupt);
            assert_ne!(UnixSignal::Interrupt, UnixSignal::Kill);
        }
    }

    // ── wait_for_capture_completion ──

    #[test]
    fn wait_for_capture_completion_noop_without_capture() {
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec!["echo".into()]),
            cwd: None,
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            containment: None,
        });
        process.wait_for_capture_completion_impl();
    }

    // ── build_command tests ──

    #[test]
    fn build_command_from_argv() {
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec!["echo".into(), "hello".into(), "world".into()]),
            cwd: None,
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            containment: None,
        });
        let cmd = process.build_command();
        assert_eq!(cmd.get_program(), "echo");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, vec!["hello", "world"]);
    }

    #[test]
    fn build_command_from_shell() {
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Shell("echo test".into()),
            cwd: None,
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            containment: None,
        });
        let cmd = process.build_command();
        // Shell commands go through the OS shell
        let program = cmd.get_program().to_string_lossy().to_string();
        #[cfg(windows)]
        assert!(
            program.contains("cmd"),
            "expected cmd shell, got {}",
            program
        );
        #[cfg(not(windows))]
        assert!(program.contains("sh"), "expected sh shell, got {}", program);
    }

    #[test]
    fn build_command_with_cwd() {
        let tmp = std::env::temp_dir();
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec!["echo".into()]),
            cwd: Some(tmp.clone()),
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            containment: None,
        });
        let cmd = process.build_command();
        assert_eq!(cmd.get_current_dir().unwrap(), &tmp);
    }

    #[test]
    fn build_command_with_env() {
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec!["echo".into()]),
            cwd: None,
            env: Some(vec![
                ("FOO".into(), "bar".into()),
                ("BAZ".into(), "qux".into()),
            ]),
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            containment: None,
        });
        let cmd = process.build_command();
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs
            .iter()
            .any(|(k, v)| *k == "FOO" && *v == Some(std::ffi::OsStr::new("bar"))));
        assert!(envs
            .iter()
            .any(|(k, v)| *k == "BAZ" && *v == Some(std::ffi::OsStr::new("qux"))));
    }

    #[test]
    fn build_command_single_argv() {
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec!["echo".into()]),
            cwd: None,
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            containment: None,
        });
        let cmd = process.build_command();
        assert_eq!(cmd.get_program(), "echo");
        assert_eq!(cmd.get_args().count(), 0);
    }

    // ── set_returncode tests ──

    #[test]
    fn set_returncode_updates_shared_state() {
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec!["echo".into()]),
            cwd: None,
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            containment: None,
        });
        assert!(process.returncode().is_none());
        process.set_returncode(42);
        assert_eq!(process.returncode(), Some(42));
    }

    #[test]
    fn set_returncode_overwrites() {
        let process = NativeProcess::new(ProcessConfig {
            command: CommandSpec::Argv(vec!["echo".into()]),
            cwd: None,
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            containment: None,
        });
        process.set_returncode(1);
        process.set_returncode(2);
        assert_eq!(process.returncode(), Some(2));
    }

    // ── SharedState with capture ──

    #[test]
    fn shared_state_with_capture_queues_open() {
        let state = SharedState::new(true);
        let guard = state.queues.lock().unwrap();
        assert!(!guard.stdout_closed);
        assert!(!guard.stderr_closed);
    }

    #[test]
    fn shared_state_without_capture_queues_closed() {
        let state = SharedState::new(false);
        let guard = state.queues.lock().unwrap();
        assert!(guard.stdout_closed);
        assert!(guard.stderr_closed);
    }

    // ── ProcessError Display additional variants ──

    #[test]
    fn process_error_display_io_variant() {
        let err = ProcessError::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "pipe broken",
        ));
        let msg = format!("{}", err);
        assert!(msg.contains("pipe broken"));
    }

    #[test]
    fn process_error_display_spawn_variant() {
        let err = ProcessError::Spawn(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not found",
        ));
        let msg = format!("{}", err);
        assert!(msg.contains("not found"));
    }

    // ── shell_command produces a command ──

    #[test]
    fn shell_command_returns_command_with_shell() {
        let cmd = shell_command("echo test");
        let program = cmd.get_program().to_string_lossy().to_string();
        #[cfg(windows)]
        assert!(program.contains("cmd"));
        #[cfg(not(windows))]
        assert!(program.contains("sh"));
    }
}
