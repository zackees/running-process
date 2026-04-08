use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

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

    pub fn start(&self) -> Result<(), ProcessError> {
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
        #[cfg(windows)]
        let job = assign_child_to_windows_kill_on_close_job(&child).map_err(ProcessError::Spawn)?;
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
        let child = &mut guard.as_mut().ok_or(ProcessError::NotRunning)?.child;
        let status = child.try_wait().map_err(ProcessError::Io)?;
        if let Some(status) = status {
            let code = exit_code(status);
            self.set_returncode(code);
            return Ok(Some(code));
        }
        Ok(None)
    }

    pub fn wait(&self, timeout: Option<Duration>) -> Result<i32, ProcessError> {
        let start = Instant::now();
        loop {
            if let Some(code) = self.poll()? {
                self.wait_for_capture_completion();
                return Ok(code);
            }
            if timeout.is_some_and(|limit| start.elapsed() >= limit) {
                return Ok(-999_999);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn kill(&self) -> Result<(), ProcessError> {
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

    pub fn read_combined(&self, timeout: Option<Duration>) -> ReadStatus<StreamEvent> {
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

    fn wait_for_capture_completion(&self) {
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

#[cfg(windows)]
fn assign_child_to_windows_kill_on_close_job(child: &Child) -> Result<WindowsJobHandle, std::io::Error> {
    use std::mem::zeroed;
    use std::os::windows::io::AsRawHandle;

    use winapi::shared::minwindef::FALSE;
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::jobapi2::{AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject};
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
