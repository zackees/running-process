use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, MasterPty};
use thiserror::Error;

/// Re-exports for downstream crates that need portable-pty types.
pub mod reexports {
    pub use portable_pty;
}

#[cfg(unix)]
pub(super) mod pty_posix;
#[cfg(windows)]
pub(super) mod pty_windows;

pub mod terminal_input;

// #150: ConPTY rewrite with PSEUDOCONSOLE_PASSTHROUGH_MODE so raw
// child ANSI bytes reach the daemon ring buffer instead of ConPTY's
// synthesized virtual-screen re-emission. Windows-only; Unix continues
// to use portable-pty via the `pty_platform = pty_posix` alias above.
#[cfg(windows)]
pub(super) mod conpty_passthrough;

// #150: backend abstraction so native_pty_process.rs calls a single
// Backend::openpty() regardless of platform.
pub(super) mod backend;

mod native_pty_process;
pub use native_pty_process::{
    InteractivePtyOptions, InteractivePtyPumpResult, InteractivePtySession, NativePtyProcess,
};

#[cfg(unix)]
use pty_posix as pty_platform;

#[derive(Debug, Error)]
pub enum PtyError {
    #[error("pseudo-terminal process already started")]
    AlreadyStarted,
    #[error("pseudo-terminal process is not running")]
    NotRunning,
    #[error("pseudo-terminal timed out")]
    Timeout,
    #[error("pseudo-terminal I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("pseudo-terminal spawn failed: {0}")]
    Spawn(String),
    #[error("pseudo-terminal error: {0}")]
    Other(String),
}

pub fn is_ignorable_process_control_error(err: &std::io::Error) -> bool {
    if matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput
    ) {
        return true;
    }
    #[cfg(unix)]
    if err.raw_os_error() == Some(libc::ESRCH) {
        return true;
    }
    false
}

pub struct PtyReadState {
    pub chunks: VecDeque<Vec<u8>>,
    pub closed: bool,
}

pub struct PtyReadShared {
    pub state: Mutex<PtyReadState>,
    pub condvar: Condvar,
}

pub struct NativePtyHandles {
    // #150: master/child were `Box<dyn portable_pty::MasterPty>` etc.
    // Refactored to use the cross-platform PtyMaster / PtyChild
    // traits so the Windows path goes through `conpty_passthrough`
    // (with PSEUDOCONSOLE_PASSTHROUGH_MODE) instead of portable-pty.
    pub master: Box<dyn crate::pty::backend::PtyMaster>,
    pub writer: Box<dyn Write + Send>,
    pub child: Box<dyn crate::pty::backend::PtyChild>,
    #[cfg(windows)]
    pub _job: WindowsJobHandle,
}

#[cfg(windows)]
pub struct WindowsJobHandle(pub usize);

#[cfg(windows)]
impl WindowsJobHandle {
    /// Assign an additional process (by PID) to this Job Object.
    pub fn assign_pid(&self, pid: u32) -> Result<(), std::io::Error> {
        use winapi::um::handleapi::CloseHandle;
        use winapi::um::processthreadsapi::OpenProcess;
        use winapi::um::winnt::PROCESS_SET_QUOTA;
        use winapi::um::winnt::PROCESS_TERMINATE;

        let handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let result = unsafe {
            winapi::um::jobapi2::AssignProcessToJobObject(
                self.0 as winapi::shared::ntdef::HANDLE,
                handle,
            )
        };
        unsafe { CloseHandle(handle) };
        if result == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for WindowsJobHandle {
    fn drop(&mut self) {
        unsafe {
            winapi::um::handleapi::CloseHandle(self.0 as winapi::shared::ntdef::HANDLE);
        }
    }
}

pub struct IdleMonitorState {
    pub last_reset_at: Instant,
    pub returncode: Option<i32>,
    pub interrupted: bool,
}

/// Core idle detection logic, shareable across threads via Arc.
/// The reader thread calls `record_output` directly.
pub struct IdleDetectorCore {
    pub timeout_seconds: f64,
    pub stability_window_seconds: f64,
    pub sample_interval_seconds: f64,
    pub reset_on_input: bool,
    pub reset_on_output: bool,
    pub count_control_churn_as_output: bool,
    pub enabled: Arc<AtomicBool>,
    pub state: Mutex<IdleMonitorState>,
    pub condvar: Condvar,
}

impl IdleDetectorCore {
    pub fn record_input(&self, byte_count: usize) {
        if !self.reset_on_input || byte_count == 0 {
            return;
        }
        let mut guard = self.state.lock().expect("idle monitor mutex poisoned");
        guard.last_reset_at = Instant::now();
        self.condvar.notify_all();
    }

    pub fn record_output(&self, data: &[u8]) {
        if !self.reset_on_output || data.is_empty() {
            return;
        }
        let control_bytes = control_churn_bytes(data);
        let visible_output_bytes = data.len().saturating_sub(control_bytes);
        let active_output =
            visible_output_bytes > 0 || (self.count_control_churn_as_output && control_bytes > 0);
        if !active_output {
            return;
        }
        let mut guard = self.state.lock().expect("idle monitor mutex poisoned");
        guard.last_reset_at = Instant::now();
        self.condvar.notify_all();
    }

    pub fn mark_exit(&self, returncode: i32, interrupted: bool) {
        let mut guard = self.state.lock().expect("idle monitor mutex poisoned");
        guard.returncode = Some(returncode);
        guard.interrupted = interrupted;
        self.condvar.notify_all();
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn set_enabled(&self, enabled: bool) {
        let was_enabled = self.enabled.swap(enabled, Ordering::AcqRel);
        if enabled && !was_enabled {
            let mut guard = self.state.lock().expect("idle monitor mutex poisoned");
            guard.last_reset_at = Instant::now();
        }
        self.condvar.notify_all();
    }

    pub fn wait(&self, timeout: Option<f64>) -> (bool, String, f64, Option<i32>) {
        let started = Instant::now();
        let overall_timeout = timeout.map(Duration::from_secs_f64);
        let min_idle = self.timeout_seconds.max(self.stability_window_seconds);
        let sample_interval = Duration::from_secs_f64(self.sample_interval_seconds.max(0.001));

        let mut guard = self.state.lock().expect("idle monitor mutex poisoned");
        loop {
            let now = Instant::now();
            let idle_for = now.duration_since(guard.last_reset_at).as_secs_f64();

            if let Some(returncode) = guard.returncode {
                let reason = if guard.interrupted {
                    "interrupt"
                } else {
                    "process_exit"
                };
                return (false, reason.to_string(), idle_for, Some(returncode));
            }

            let enabled = self.enabled.load(Ordering::Acquire);
            if enabled && idle_for >= min_idle {
                return (true, "idle_timeout".to_string(), idle_for, None);
            }

            if let Some(limit) = overall_timeout {
                if now.duration_since(started) >= limit {
                    return (false, "timeout".to_string(), idle_for, None);
                }
            }

            let idle_remaining = if enabled {
                (min_idle - idle_for).max(0.0)
            } else {
                sample_interval.as_secs_f64()
            };
            let mut wait_for =
                sample_interval.min(Duration::from_secs_f64(idle_remaining.max(0.001)));
            if let Some(limit) = overall_timeout {
                let elapsed = now.duration_since(started);
                if elapsed < limit {
                    let remaining = limit - elapsed;
                    wait_for = wait_for.min(remaining);
                }
            }
            let result = self
                .condvar
                .wait_timeout(guard, wait_for)
                .expect("idle monitor mutex poisoned");
            guard = result.0;
        }
    }
}


// ── Helper functions ──

pub fn control_churn_bytes(data: &[u8]) -> usize {
    let mut total = 0;
    let mut index = 0;
    while index < data.len() {
        let byte = data[index];
        if byte == 0x1B {
            let start = index;
            index += 1;
            if index < data.len() && data[index] == b'[' {
                index += 1;
                while index < data.len() {
                    let current = data[index];
                    index += 1;
                    if (0x40..=0x7E).contains(&current) {
                        break;
                    }
                }
            }
            total += index - start;
            continue;
        }
        if matches!(byte, 0x08 | 0x0D | 0x7F) {
            total += 1;
        }
        index += 1;
    }
    total
}

pub fn command_builder_from_argv(argv: &[String]) -> CommandBuilder {
    let mut command = CommandBuilder::new(&argv[0]);
    if argv.len() > 1 {
        command.args(
            argv[1..]
                .iter()
                .map(OsString::from)
                .collect::<Vec<OsString>>(),
        );
    }
    command
}

#[inline(never)]
pub fn spawn_pty_reader(
    mut reader: Box<dyn Read + Send>,
    shared: Arc<PtyReadShared>,
    echo: Arc<AtomicBool>,
    idle_detector: Arc<Mutex<Option<Arc<IdleDetectorCore>>>>,
    output_bytes_total: Arc<AtomicUsize>,
    control_churn_bytes_total: Arc<AtomicUsize>,
) {
    crate::rp_rust_debug_scope!("running_process::spawn_pty_reader");
    let idle_detector_snapshot = idle_detector
        .lock()
        .expect("idle detector mutex poisoned")
        .clone();
    let mut chunk = vec![0_u8; 65536];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let data = &chunk[..n];

                let churn = control_churn_bytes(data);
                let visible = data.len().saturating_sub(churn);
                output_bytes_total.fetch_add(visible, Ordering::Relaxed);
                control_churn_bytes_total.fetch_add(churn, Ordering::Relaxed);

                if echo.load(Ordering::Relaxed) {
                    let _ = std::io::stdout().write_all(data);
                    let _ = std::io::stdout().flush();
                }

                if let Some(ref detector) = idle_detector_snapshot {
                    detector.record_output(data);
                }

                let mut guard = shared.state.lock().expect("pty read mutex poisoned");
                guard.chunks.push_back(data.to_vec());
                shared.condvar.notify_all();
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                // #199: intentional — back-off on a non-blocking PTY
                // master read that returned WouldBlock. There's no
                // POSIX "wait for fd readable" that's portable
                // across the OwnedFd / Windows OwnedHandle paths
                // used here.
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(_) => break,
        }
    }
    let mut guard = shared.state.lock().expect("pty read mutex poisoned");
    guard.closed = true;
    shared.condvar.notify_all();
}

pub fn portable_exit_code(status: portable_pty::ExitStatus) -> i32 {
    if let Some(signal) = status.signal() {
        let signal = signal.to_ascii_lowercase();
        if signal.contains("interrupt") {
            return -2;
        }
        if signal.contains("terminated") {
            return -15;
        }
        if signal.contains("killed") {
            return -9;
        }
    }
    status.exit_code() as i32
}

pub fn input_contains_newline(data: &[u8]) -> bool {
    data.iter().any(|byte| matches!(*byte, b'\r' | b'\n'))
}

#[cfg(unix)]
struct PosixTerminalModeGuard {
    stdin_fd: i32,
    original_mode: libc::termios,
}

#[cfg(unix)]
impl Drop for PosixTerminalModeGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.stdin_fd, libc::TCSANOW, &self.original_mode);
        }
    }
}

#[cfg(unix)]
fn acquire_posix_terminal_mode_guard() -> Result<PosixTerminalModeGuard, std::io::Error> {
    let stdin_fd = libc::STDIN_FILENO;
    let mut original_mode = unsafe { std::mem::zeroed::<libc::termios>() };
    if unsafe { libc::tcgetattr(stdin_fd, &mut original_mode) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut raw_mode = original_mode;
    unsafe {
        libc::cfmakeraw(&mut raw_mode);
    }
    if unsafe { libc::tcsetattr(stdin_fd, libc::TCSANOW, &raw_mode) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(PosixTerminalModeGuard {
        stdin_fd,
        original_mode,
    })
}

#[cfg(unix)]
#[inline(never)]
pub(super) fn posix_terminal_input_relay_worker(
    handles: Arc<Mutex<Option<NativePtyHandles>>>,
    returncode: Arc<Mutex<Option<i32>>>,
    input_bytes_total: Arc<AtomicUsize>,
    newline_events_total: Arc<AtomicUsize>,
    submit_events_total: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
) {
    let _terminal_guard = match acquire_posix_terminal_mode_guard() {
        Ok(guard) => guard,
        Err(_) => {
            active.store(false, Ordering::Release);
            return;
        }
    };

    let stdin_fd = libc::STDIN_FILENO;
    let mut buffer = vec![0_u8; 65536];
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        match poll_pty_process(&handles, &returncode) {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => break,
        }

        let mut pollfd = libc::pollfd {
            fd: stdin_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let poll_result = unsafe { libc::poll(&mut pollfd, 1, 50) };
        if poll_result < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if poll_result == 0 || pollfd.revents & libc::POLLIN == 0 {
            continue;
        }

        let read_result = unsafe { libc::read(stdin_fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read_result < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if read_result == 0 {
            continue;
        }

        let mut data = buffer[..read_result as usize].to_vec();
        loop {
            let mut drain_pollfd = libc::pollfd {
                fd: stdin_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let drain_ready = unsafe { libc::poll(&mut drain_pollfd, 1, 0) };
            if drain_ready <= 0 || drain_pollfd.revents & libc::POLLIN == 0 {
                break;
            }
            let drain_result =
                unsafe { libc::read(stdin_fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if drain_result <= 0 {
                break;
            }
            data.extend_from_slice(&buffer[..drain_result as usize]);
        }

        record_pty_input_metrics(
            &input_bytes_total,
            &newline_events_total,
            &submit_events_total,
            &data,
            input_contains_newline(&data),
        );
        if write_pty_input(&handles, &data).is_err() {
            break;
        }
    }

    active.store(false, Ordering::Release);
}

pub fn record_pty_input_metrics(
    input_bytes_total: &Arc<AtomicUsize>,
    newline_events_total: &Arc<AtomicUsize>,
    submit_events_total: &Arc<AtomicUsize>,
    data: &[u8],
    submit: bool,
) {
    input_bytes_total.fetch_add(data.len(), Ordering::AcqRel);
    if input_contains_newline(data) {
        newline_events_total.fetch_add(1, Ordering::AcqRel);
    }
    if submit {
        submit_events_total.fetch_add(1, Ordering::AcqRel);
    }
}

pub fn store_pty_returncode(returncode: &Arc<Mutex<Option<i32>>>, code: i32) {
    *returncode.lock().expect("pty returncode mutex poisoned") = Some(code);
}

pub fn poll_pty_process(
    handles: &Arc<Mutex<Option<NativePtyHandles>>>,
    returncode: &Arc<Mutex<Option<i32>>>,
) -> Result<Option<i32>, std::io::Error> {
    let mut guard = handles.lock().expect("pty handles mutex poisoned");
    let Some(handles) = guard.as_mut() else {
        return Ok(*returncode.lock().expect("pty returncode mutex poisoned"));
    };
    let status = handles.child.try_wait()?;
    // #150: try_wait now returns Option<u32> (from PtyChild trait)
    // instead of portable_pty's ExitStatus. Just cast for storage.
    let code = status.map(|c| c as i32);
    if let Some(code) = code {
        store_pty_returncode(returncode, code);
        return Ok(Some(code));
    }
    Ok(None)
}

pub fn write_pty_input(
    handles: &Arc<Mutex<Option<NativePtyHandles>>>,
    data: &[u8],
) -> Result<(), std::io::Error> {
    let mut guard = handles.lock().expect("pty handles mutex poisoned");
    let handles = guard.as_mut().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "Pseudo-terminal process is not running",
        )
    })?;
    #[cfg(windows)]
    let payload = pty_windows::input_payload(data);
    #[cfg(unix)]
    let payload = pty_platform::input_payload(data);
    handles.writer.write_all(&payload)?;
    handles.writer.flush()
}

#[cfg(windows)]
pub fn windows_terminal_input_payload(data: &[u8]) -> Vec<u8> {
    let mut translated = Vec::with_capacity(data.len());
    let mut index = 0usize;
    while index < data.len() {
        let current = data[index];
        if current == b'\r' {
            translated.push(current);
            if index + 1 < data.len() && data[index + 1] == b'\n' {
                translated.push(b'\n');
                index += 2;
                continue;
            }
            index += 1;
            continue;
        }
        if current == b'\n' {
            translated.push(b'\r');
            index += 1;
            continue;
        }
        translated.push(current);
        index += 1;
    }
    translated
}

#[cfg(windows)]
#[inline(never)]
pub fn assign_child_to_windows_kill_on_close_job(
    handle: Option<std::os::windows::io::RawHandle>,
) -> Result<WindowsJobHandle, PtyError> {
    crate::rp_rust_debug_scope!(
        "running_process::pty::assign_child_to_windows_kill_on_close_job"
    );
    use std::mem::zeroed;

    use winapi::shared::minwindef::FALSE;
    use winapi::um::handleapi::INVALID_HANDLE_VALUE;
    use winapi::um::jobapi2::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    };
    use winapi::um::winnt::{
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    let Some(handle) = handle else {
        return Err(PtyError::Other(
            "Pseudo-terminal child does not expose a Windows process handle".into(),
        ));
    };

    let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
    if job.is_null() || job == INVALID_HANDLE_VALUE {
        return Err(PtyError::Io(std::io::Error::last_os_error()));
    }

    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let result = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if result == FALSE {
        let err = std::io::Error::last_os_error();
        unsafe {
            winapi::um::handleapi::CloseHandle(job);
        }
        return Err(PtyError::Io(err));
    }

    let result = unsafe { AssignProcessToJobObject(job, handle.cast()) };
    if result == FALSE {
        let err = std::io::Error::last_os_error();
        unsafe {
            winapi::um::handleapi::CloseHandle(job);
        }
        return Err(PtyError::Io(err));
    }

    Ok(WindowsJobHandle(job as usize))
}

/// Information about a child process found via Toolhelp snapshot.
#[cfg(windows)]
#[derive(Debug, Clone)]
pub struct ChildProcessInfo {
    pub pid: u32,
    pub name: String,
}

/// Find all direct child processes of a given parent PID using the Windows Toolhelp API.
/// Returns PID and process name for each child.
#[cfg(windows)]
pub fn find_child_processes(parent_pid: u32) -> Vec<ChildProcessInfo> {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::tlhelp32::{
        CreateToolhelp32Snapshot, Process32First, Process32Next, PROCESSENTRY32, TH32CS_SNAPPROCESS,
    };

    let mut children = Vec::new();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == winapi::um::handleapi::INVALID_HANDLE_VALUE {
        return children;
    }

    let mut entry: PROCESSENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;

    if unsafe { Process32First(snapshot, &mut entry) } != 0 {
        loop {
            if entry.th32ParentProcessID == parent_pid {
                let name_bytes = &entry.szExeFile;
                let name_len = name_bytes
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(name_bytes.len());
                let name = String::from_utf8_lossy(
                    &name_bytes[..name_len]
                        .iter()
                        .map(|&c| c as u8)
                        .collect::<Vec<u8>>(),
                )
                .into_owned();
                children.push(ChildProcessInfo {
                    pid: entry.th32ProcessID,
                    name,
                });
            }
            if unsafe { Process32Next(snapshot, &mut entry) } == 0 {
                break;
            }
        }
    }

    unsafe { CloseHandle(snapshot) };
    children
}

/// Return PIDs of all conhost.exe processes that are children of the current process.
#[cfg(windows)]
pub(super) fn conhost_children_of_current_process() -> Vec<u32> {
    let our_pid = std::process::id();
    find_child_processes(our_pid)
        .into_iter()
        .filter(|c| c.name.eq_ignore_ascii_case("conhost.exe"))
        .map(|c| c.pid)
        .collect()
}

/// After spawning a ConPTY child, find the new conhost.exe process that was created
/// by the ConPTY infrastructure (child of our process, not present in the "before"
/// snapshot) and assign it to the Job Object so it gets cleaned up on Job close.
#[cfg(windows)]
pub(super) fn assign_conpty_conhost_to_job(job: &WindowsJobHandle, before_pids: &[u32]) {
    let after_pids = conhost_children_of_current_process();
    for pid in after_pids {
        if !before_pids.contains(&pid) {
            // This is a newly created conhost.exe — assign it to the Job.
            let _ = job.assign_pid(pid);
        }
    }
}

/// A conhost.exe process whose parent is no longer alive — likely an orphan
/// from a dead ConPTY session.
#[cfg(windows)]
#[derive(Debug, Clone)]
pub struct OrphanConhostInfo {
    /// PID of the orphaned conhost.exe.
    pub pid: u32,
    /// PID that was the parent when the snapshot was taken.
    pub parent_pid: u32,
    /// Name of the parent process, if it can be resolved (empty if parent is dead).
    pub parent_name: String,
}

/// Scan all conhost.exe processes on the system and return those whose parent
/// process is no longer alive. These are likely orphans from dead ConPTY sessions.
///
/// Uses `CreateToolhelp32Snapshot` for a point-in-time snapshot — no sysinfo
/// dependency, so it's lightweight and can be called frequently.
#[cfg(windows)]
pub fn find_orphan_conhosts() -> Vec<OrphanConhostInfo> {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::tlhelp32::{
        CreateToolhelp32Snapshot, Process32First, Process32Next, PROCESSENTRY32, TH32CS_SNAPPROCESS,
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == winapi::um::handleapi::INVALID_HANDLE_VALUE {
        return Vec::new();
    }

    let mut entry: PROCESSENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;

    // First pass: collect all PIDs and identify conhost.exe processes.
    let mut all_pids = std::collections::HashSet::new();
    let mut conhosts: Vec<(u32, u32)> = Vec::new(); // (pid, parent_pid)
    let mut parent_names: std::collections::HashMap<u32, String> = std::collections::HashMap::new();

    if unsafe { Process32First(snapshot, &mut entry) } != 0 {
        loop {
            let name_bytes = &entry.szExeFile;
            let name_len = name_bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(name_bytes.len());
            let name = String::from_utf8_lossy(
                &name_bytes[..name_len]
                    .iter()
                    .map(|&c| c as u8)
                    .collect::<Vec<u8>>(),
            )
            .into_owned();

            all_pids.insert(entry.th32ProcessID);
            parent_names.insert(entry.th32ProcessID, name.clone());

            if name.eq_ignore_ascii_case("conhost.exe") {
                conhosts.push((entry.th32ProcessID, entry.th32ParentProcessID));
            }

            if unsafe { Process32Next(snapshot, &mut entry) } == 0 {
                break;
            }
        }
    }

    unsafe { CloseHandle(snapshot) };

    // Second pass: filter to conhosts whose parent PID is not in the live set.
    conhosts
        .into_iter()
        .filter(|&(_, parent_pid)| !all_pids.contains(&parent_pid))
        .map(|(pid, parent_pid)| OrphanConhostInfo {
            pid,
            parent_pid,
            parent_name: parent_names.get(&parent_pid).cloned().unwrap_or_default(),
        })
        .collect()
}

#[cfg(windows)]
#[inline(never)]
pub fn apply_windows_pty_priority(
    handle: Option<std::os::windows::io::RawHandle>,
    nice: Option<i32>,
) -> Result<(), PtyError> {
    crate::rp_rust_debug_scope!("running_process::pty::apply_windows_pty_priority");
    use winapi::um::processthreadsapi::SetPriorityClass;
    use winapi::um::winbase::{
        ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS,
        IDLE_PRIORITY_CLASS,
    };

    let Some(handle) = handle else {
        return Ok(());
    };
    let flags = match nice {
        Some(value) if value >= 15 => IDLE_PRIORITY_CLASS,
        Some(value) if value >= 1 => BELOW_NORMAL_PRIORITY_CLASS,
        Some(value) if value <= -15 => HIGH_PRIORITY_CLASS,
        Some(value) if value <= -1 => ABOVE_NORMAL_PRIORITY_CLASS,
        _ => 0,
    };
    if flags == 0 {
        return Ok(());
    }
    let result = unsafe { SetPriorityClass(handle.cast(), flags) };
    if result == 0 {
        return Err(PtyError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::native_pty_process::resolved_spawn_cwd;

    #[test]
    fn resolved_spawn_cwd_preserves_explicit_value() {
        assert_eq!(
            resolved_spawn_cwd(Some("C:\\temp\\explicit")),
            Some("C:\\temp\\explicit".to_string())
        );
    }

    #[test]
    fn resolved_spawn_cwd_defaults_to_current_dir_when_unset() {
        let expected = std::env::current_dir()
            .ok()
            .map(|cwd| cwd.to_string_lossy().to_string());
        assert_eq!(resolved_spawn_cwd(None), expected);
    }
}
