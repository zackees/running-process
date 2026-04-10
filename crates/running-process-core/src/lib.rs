use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

mod public_symbols;
mod rust_debug;

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
    pub creationflags: Option<u32>,
    pub create_process_group: bool,
    pub stdin_mode: StdinMode,
    pub nice: Option<i32>,
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

struct SharedState {
    queues: Mutex<QueueState>,
    condvar: Condvar,
    returncode: Mutex<Option<i32>>,
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
            returncode: Mutex::new(None),
        }
    }
}

pub struct NativeProcess {
    config: ProcessConfig,
    child: Mutex<Option<ChildState>>,
    shared: Arc<SharedState>,
}

impl NativeProcess {
    pub fn new(config: ProcessConfig) -> Self {
        Self {
            shared: Arc::new(SharedState::new(config.capture)),
            child: Mutex::new(None),
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
            self.spawn_reader(stdout, StreamKind::Stdout);
            self.spawn_reader(stderr, StreamKind::Stderr);
        }
        *guard = Some(ChildState {
            child,
            #[cfg(windows)]
            _job: job,
        });
        Ok(())
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
        *self
            .shared
            .returncode
            .lock()
            .expect("returncode mutex poisoned")
    }

    pub fn has_pending_stream(&self, stream: StreamKind) -> bool {
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
            let queue = match stream {
                StreamKind::Stdout => &mut guard.stdout_queue,
                StreamKind::Stderr => &mut guard.stderr_queue,
            };
            if let Some(line) = queue.pop_front() {
                return ReadStatus::Line(line);
            }

            let closed = match stream {
                StreamKind::Stdout => guard.stdout_closed,
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
        if self.config.create_process_group || self.config.nice.is_some() {
            use std::os::unix::process::CommandExt;
            let create_process_group = self.config.create_process_group;
            let nice = self.config.nice;

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
        command
    }

    fn spawn_reader<R>(&self, pipe: R, stream: StreamKind)
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
                    Ok(n) => feed_chunk(&shared, stream, &mut pending, &chunk[..n]),
                    Err(_) => break,
                }
            }

            if !pending.is_empty() {
                emit_line(&shared, stream, std::mem::take(&mut pending));
            }

            let mut guard = shared.queues.lock().expect("queue mutex poisoned");
            match stream {
                StreamKind::Stdout => guard.stdout_closed = true,
                StreamKind::Stderr => guard.stderr_closed = true,
            }
            shared.condvar.notify_all();
        });
    }

    fn set_returncode(&self, code: i32) {
        let mut guard = self
            .shared
            .returncode
            .lock()
            .expect("returncode mutex poisoned");
        *guard = Some(code);
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
