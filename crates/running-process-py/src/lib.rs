use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::{Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use pyo3::exceptions::{PyRuntimeError, PyTimeoutError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyString};
use regex::Regex;
use running_process_core::{
    render_rust_debug_traces, CommandSpec, NativeProcess, ProcessConfig, ProcessError, ReadStatus,
    StdinMode, StreamEvent, StreamKind,
};
#[cfg(unix)]
use running_process_core::{
    unix_set_priority, unix_signal_process, unix_signal_process_group, UnixSignal,
};
use sysinfo::{Pid, ProcessRefreshKind, Signal, System, UpdateKind};

#[cfg(unix)]
mod pty_posix;
mod public_symbols;
#[cfg(windows)]
mod pty_windows;

#[cfg(unix)]
use pty_posix as pty_platform;

fn to_py_err(err: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(err.to_string())
}

fn is_ignorable_process_control_error(err: &std::io::Error) -> bool {
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

fn process_err_to_py(err: ProcessError) -> PyErr {
    match err {
        ProcessError::Timeout => PyTimeoutError::new_err("process timed out"),
        other => to_py_err(other),
    }
}

fn system_pid(pid: u32) -> Pid {
    Pid::from_u32(pid)
}

fn descendant_pids(system: &System, pid: Pid) -> Vec<Pid> {
    let mut descendants = Vec::new();
    let mut stack = vec![pid];
    while let Some(current) = stack.pop() {
        for (child_pid, process) in system.processes() {
            if process.parent() == Some(current) {
                descendants.push(*child_pid);
                stack.push(*child_pid);
            }
        }
    }
    descendants
}

#[derive(Clone)]
struct ActiveProcessRecord {
    pid: u32,
    kind: String,
    command: String,
    cwd: Option<String>,
    started_at: f64,
}

type TrackedProcessEntry = (u32, f64, String, String, Option<String>);
type ActiveProcessEntry = (u32, String, String, Option<String>, f64);
type ExpectDetails = (String, usize, usize, Vec<String>);
type ExpectResult = (
    String,
    String,
    Option<String>,
    Option<usize>,
    Option<usize>,
    Vec<String>,
);

fn active_process_registry() -> &'static Mutex<HashMap<u32, ActiveProcessRecord>> {
    static ACTIVE_PROCESSES: OnceLock<Mutex<HashMap<u32, ActiveProcessRecord>>> = OnceLock::new();
    ACTIVE_PROCESSES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn unix_now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

fn register_active_process(
    pid: u32,
    kind: &str,
    command: &str,
    cwd: Option<String>,
    started_at: f64,
) {
    let mut registry = active_process_registry()
        .lock()
        .expect("active process registry mutex poisoned");
    registry.insert(
        pid,
        ActiveProcessRecord {
            pid,
            kind: kind.to_string(),
            command: command.to_string(),
            cwd,
            started_at,
        },
    );
}

fn unregister_active_process(pid: u32) {
    let mut registry = active_process_registry()
        .lock()
        .expect("active process registry mutex poisoned");
    registry.remove(&pid);
}

fn process_created_at(pid: u32) -> Option<f64> {
    let pid = system_pid(pid);
    let mut system = System::new();
    system.refresh_process_specifics(
        pid,
        ProcessRefreshKind::new()
            .with_cpu()
            .with_disk_usage()
            .with_memory()
            .with_exe(UpdateKind::Never),
    );
    system
        .process(pid)
        .map(|process| process.start_time() as f64)
}

fn same_process_identity(pid: u32, created_at: f64, tolerance_seconds: f64) -> bool {
    let Some(actual) = process_created_at(pid) else {
        return false;
    };
    (actual - created_at).abs() <= tolerance_seconds
}

fn tracked_process_db_path() -> PyResult<PathBuf> {
    if let Ok(value) = std::env::var("RUNNING_PROCESS_PID_DB") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }

    #[cfg(windows)]
    let base_dir = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);

    #[cfg(not(windows))]
    let base_dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                let mut path = PathBuf::from(home);
                path.push(".local");
                path.push("state");
                path
            })
        })
        .unwrap_or_else(std::env::temp_dir);

    Ok(base_dir
        .join("running-process")
        .join("tracked-pids.sqlite3"))
}

#[pyfunction]
fn tracked_pid_db_path_py() -> PyResult<String> {
    Ok(tracked_process_db_path()?.to_string_lossy().into_owned())
}

#[pyfunction]
#[pyo3(signature = (pid, created_at, kind, command, cwd=None))]
fn track_process_pid(
    pid: u32,
    created_at: f64,
    kind: &str,
    command: &str,
    cwd: Option<String>,
) -> PyResult<()> {
    let _ = created_at;
    register_active_process(pid, kind, command, cwd, unix_now_seconds());
    Ok(())
}

#[pyfunction]
#[pyo3(signature = (pid, kind, command, cwd=None))]
fn native_register_process(
    pid: u32,
    kind: &str,
    command: &str,
    cwd: Option<String>,
) -> PyResult<()> {
    register_active_process(pid, kind, command, cwd, unix_now_seconds());
    Ok(())
}

#[pyfunction]
fn untrack_process_pid(pid: u32) -> PyResult<()> {
    unregister_active_process(pid);
    Ok(())
}

#[pyfunction]
fn native_unregister_process(pid: u32) -> PyResult<()> {
    unregister_active_process(pid);
    Ok(())
}

#[pyfunction]
fn list_tracked_processes() -> PyResult<Vec<TrackedProcessEntry>> {
    let registry = active_process_registry()
        .lock()
        .expect("active process registry mutex poisoned");
    let mut entries: Vec<_> = registry
        .values()
        .map(|entry| {
            (
                entry.pid,
                process_created_at(entry.pid).unwrap_or(entry.started_at),
                entry.kind.clone(),
                entry.command.clone(),
                entry.cwd.clone(),
            )
        })
        .collect();
    entries.sort_by(|left, right| {
        left.1
            .partial_cmp(&right.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });
    Ok(entries)
}

fn kill_process_tree_impl(pid: u32, timeout_seconds: f64) {
    let mut system = System::new_all();
    let pid = system_pid(pid);
    let Some(_) = system.process(pid) else {
        return;
    };

    let mut kill_order = descendant_pids(&system, pid);
    kill_order.reverse();
    kill_order.push(pid);

    for target in &kill_order {
        if let Some(process) = system.process(*target) {
            if !process.kill_with(Signal::Kill).unwrap_or(false) {
                process.kill();
            }
        }
    }

    let deadline = Instant::now()
        .checked_add(Duration::from_secs_f64(timeout_seconds.max(0.0)))
        .unwrap_or_else(Instant::now);
    loop {
        system.refresh_processes();
        if kill_order
            .iter()
            .all(|target| system.process(*target).is_none())
        {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(windows)]
fn windows_terminal_input_payload(data: &[u8]) -> Vec<u8> {
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

#[pyfunction]
fn native_get_process_tree_info(pid: u32) -> String {
    let system = System::new_all();
    let pid = system_pid(pid);
    let Some(process) = system.process(pid) else {
        return format!("Could not get process info for PID {}", pid.as_u32());
    };

    let mut info = vec![
        format!("Process {} ({})", pid.as_u32(), process.name()),
        format!("Status: {:?}", process.status()),
    ];
    let children = descendant_pids(&system, pid);
    if !children.is_empty() {
        info.push("Child processes:".to_string());
        for child_pid in children {
            if let Some(child) = system.process(child_pid) {
                info.push(format!("  Child {} ({})", child_pid.as_u32(), child.name()));
            }
        }
    }
    info.join("\n")
}

#[pyfunction]
#[pyo3(signature = (pid, timeout_seconds=3.0))]
fn native_kill_process_tree(pid: u32, timeout_seconds: f64) {
    kill_process_tree_impl(pid, timeout_seconds);
}

#[pyfunction]
fn native_process_created_at(pid: u32) -> Option<f64> {
    process_created_at(pid)
}

#[pyfunction]
#[pyo3(signature = (pid, created_at, tolerance_seconds=1.0))]
fn native_is_same_process(pid: u32, created_at: f64, tolerance_seconds: f64) -> bool {
    same_process_identity(pid, created_at, tolerance_seconds)
}

#[pyfunction]
#[pyo3(signature = (tolerance_seconds=1.0, kill_timeout_seconds=3.0))]
fn native_cleanup_tracked_processes(
    tolerance_seconds: f64,
    kill_timeout_seconds: f64,
) -> PyResult<Vec<TrackedProcessEntry>> {
    let entries = list_tracked_processes()?;

    let mut killed = Vec::new();
    for entry in entries {
        let pid = entry.0;
        if !same_process_identity(pid, entry.1, tolerance_seconds) {
            unregister_active_process(pid);
            continue;
        }
        kill_process_tree_impl(pid, kill_timeout_seconds);
        unregister_active_process(pid);
        killed.push(entry);
    }
    Ok(killed)
}

#[pyfunction]
fn native_list_active_processes() -> Vec<ActiveProcessEntry> {
    let registry = active_process_registry()
        .lock()
        .expect("active process registry mutex poisoned");
    let mut items: Vec<_> = registry
        .values()
        .map(|entry| {
            (
                entry.pid,
                entry.kind.clone(),
                entry.command.clone(),
                entry.cwd.clone(),
                entry.started_at,
            )
        })
        .collect();
    items.sort_by(|left, right| {
        left.4
            .partial_cmp(&right.4)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });
    items
}

#[pyfunction]
#[inline(never)]
fn native_apply_process_nice(pid: u32, nice: i32) -> PyResult<()> {
    public_symbols::rp_native_apply_process_nice_public(pid, nice)
}

fn native_apply_process_nice_impl(pid: u32, nice: i32) -> PyResult<()> {
    running_process_core::rp_rust_debug_scope!("running_process_py::native_apply_process_nice");
    #[cfg(windows)]
    {
        return public_symbols::rp_windows_apply_process_priority_public(pid, nice);
    }

    #[cfg(unix)]
    {
        unix_set_priority(pid, nice).map_err(to_py_err)
    }
}

#[cfg(windows)]
fn windows_apply_process_priority_impl(pid: u32, nice: i32) -> PyResult<()> {
    running_process_core::rp_rust_debug_scope!(
        "running_process_py::windows_apply_process_priority"
    );
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::processthreadsapi::{OpenProcess, SetPriorityClass};
    use winapi::um::winbase::{
        ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS,
        IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
    };
    use winapi::um::winnt::{PROCESS_QUERY_INFORMATION, PROCESS_SET_INFORMATION};

    let priority_class = if nice >= 15 {
        IDLE_PRIORITY_CLASS
    } else if nice >= 1 {
        BELOW_NORMAL_PRIORITY_CLASS
    } else if nice <= -15 {
        HIGH_PRIORITY_CLASS
    } else if nice <= -1 {
        ABOVE_NORMAL_PRIORITY_CLASS
    } else {
        NORMAL_PRIORITY_CLASS
    };

    let handle =
        unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_SET_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(to_py_err(std::io::Error::last_os_error()));
    }
    let result = unsafe { SetPriorityClass(handle, priority_class) };
    let close_result = unsafe { CloseHandle(handle) };
    if close_result == 0 {
        return Err(to_py_err(std::io::Error::last_os_error()));
    }
    if result == 0 {
        return Err(to_py_err(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(windows)]
fn windows_generate_console_ctrl_break_impl(
    pid: u32,
    creationflags: Option<u32>,
) -> PyResult<()> {
    running_process_core::rp_rust_debug_scope!(
        "running_process_py::windows_generate_console_ctrl_break"
    );
    use winapi::um::wincon::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};

    let new_process_group =
        creationflags.unwrap_or(0) & winapi::um::winbase::CREATE_NEW_PROCESS_GROUP;
    if new_process_group == 0 {
        return Err(PyRuntimeError::new_err(
            "send_interrupt on Windows requires CREATE_NEW_PROCESS_GROUP",
        ));
    }
    let result = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
    if result == 0 {
        return Err(to_py_err(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[pyfunction]
fn native_windows_terminal_input_bytes(py: Python<'_>, data: &[u8]) -> Py<PyAny> {
    #[cfg(windows)]
    let payload = windows_terminal_input_payload(data);
    #[cfg(not(windows))]
    let payload = data.to_vec();
    PyBytes::new(py, &payload).into_any().unbind()
}

#[pyfunction]
fn native_dump_rust_debug_traces() -> String {
    render_rust_debug_traces()
}

#[pyfunction]
fn native_test_capture_rust_debug_trace() -> String {
    #[inline(never)]
    fn inner() -> String {
        running_process_core::rp_rust_debug_scope!(
            "running_process_py::native_test_capture_rust_debug_trace::inner"
        );
        render_rust_debug_traces()
    }

    #[inline(never)]
    fn outer() -> String {
        running_process_core::rp_rust_debug_scope!(
            "running_process_py::native_test_capture_rust_debug_trace::outer"
        );
        inner()
    }

    outer()
}

#[cfg(windows)]
#[no_mangle]
#[inline(never)]
pub fn running_process_py_debug_hang_inner(release_path: &std::path::Path) -> PyResult<()> {
    running_process_core::rp_rust_debug_scope!("running_process_py::debug_hang_inner");
    while !release_path.exists() {
        std::hint::spin_loop();
    }
    Ok(())
}

#[cfg(windows)]
#[no_mangle]
#[inline(never)]
pub fn running_process_py_debug_hang_outer(
    ready_path: &std::path::Path,
    release_path: &std::path::Path,
) -> PyResult<()> {
    running_process_core::rp_rust_debug_scope!("running_process_py::debug_hang_outer");
    fs::write(ready_path, b"ready").map_err(to_py_err)?;
    running_process_py_debug_hang_inner(release_path)
}

#[pyfunction]
#[cfg(windows)]
#[inline(never)]
fn native_test_hang_in_rust(ready_path: String, release_path: String) -> PyResult<()> {
    running_process_core::rp_rust_debug_scope!("running_process_py::native_test_hang_in_rust");
    running_process_py_debug_hang_outer(
        std::path::Path::new(&ready_path),
        std::path::Path::new(&release_path),
    )
}

#[pymethods]
impl NativeProcessMetrics {
    #[new]
    fn new(pid: u32) -> Self {
        let pid = system_pid(pid);
        let mut system = System::new();
        system.refresh_process_specifics(
            pid,
            ProcessRefreshKind::new()
                .with_cpu()
                .with_disk_usage()
                .with_memory()
                .with_exe(UpdateKind::Never),
        );
        Self {
            pid,
            system: Mutex::new(system),
        }
    }

    fn prime(&self) {
        let mut system = self.system.lock().expect("process metrics mutex poisoned");
        system.refresh_process_specifics(
            self.pid,
            ProcessRefreshKind::new()
                .with_cpu()
                .with_disk_usage()
                .with_memory()
                .with_exe(UpdateKind::Never),
        );
    }

    fn sample(&self) -> (bool, f32, u64, u64) {
        let mut system = self.system.lock().expect("process metrics mutex poisoned");
        system.refresh_process_specifics(
            self.pid,
            ProcessRefreshKind::new()
                .with_cpu()
                .with_disk_usage()
                .with_memory()
                .with_exe(UpdateKind::Never),
        );
        let Some(process) = system.process(self.pid) else {
            return (false, 0.0, 0, 0);
        };
        let disk = process.disk_usage();
        (
            true,
            process.cpu_usage(),
            disk.total_read_bytes
                .saturating_add(disk.total_written_bytes),
            0,
        )
    }
}

struct PtyReadState {
    chunks: VecDeque<Vec<u8>>,
    closed: bool,
}

struct PtyReadShared {
    state: Mutex<PtyReadState>,
    condvar: Condvar,
}

struct NativePtyHandles {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    #[cfg(windows)]
    _job: WindowsJobHandle,
}

#[pyclass]
struct NativeProcessMetrics {
    pid: Pid,
    system: Mutex<System>,
}

#[pyclass]
struct NativePtyProcess {
    argv: Vec<String>,
    cwd: Option<String>,
    env: Option<Vec<(String, String)>>,
    rows: u16,
    cols: u16,
    #[cfg(windows)]
    nice: Option<i32>,
    handles: Mutex<Option<NativePtyHandles>>,
    reader: Arc<PtyReadShared>,
    returncode: Mutex<Option<i32>>,
}

impl NativePtyProcess {
    fn mark_reader_closed(&self) {
        let mut guard = self.reader.state.lock().expect("pty read mutex poisoned");
        guard.closed = true;
        self.reader.condvar.notify_all();
    }

    fn store_returncode(&self, code: i32) {
        *self
            .returncode
            .lock()
            .expect("pty returncode mutex poisoned") = Some(code);
    }

    /// Synchronously tear down the PTY and reap the child.
    ///
    /// This MUST NOT be called while holding the Python GIL — `child.wait()`
    /// can block indefinitely on Windows ConPTY (the child stays alive until
    /// every handle to the master pipe is dropped, including the one held by
    /// the background reader thread). Always wrap this in `py.allow_threads`
    /// from a Python-callable method.
    // Preserve a stable Rust frame here in release user dumps.
    #[inline(never)]
    fn close_impl(&self) -> PyResult<()> {
        running_process_core::rp_rust_debug_scope!(
            "running_process_py::NativePtyProcess::close_impl"
        );
        let mut guard = self.handles.lock().expect("pty handles mutex poisoned");
        let Some(handles) = guard.take() else {
            self.mark_reader_closed();
            return Ok(());
        };
        // Release the lock while we wait so other threads can still touch
        // unrelated fields on this object (e.g. the reader buffer).
        drop(guard);

        #[cfg(windows)]
        let NativePtyHandles {
            master,
            writer,
            mut child,
            _job,
        } = handles;
        #[cfg(not(windows))]
        let NativePtyHandles {
            master,
            writer,
            mut child,
        } = handles;

        // Kill first so the child has stopped writing before we tear down
        // ConPTY. On Windows, ClosePseudoConsole (triggered by dropping
        // master) does not always terminate the child, so we explicitly
        // TerminateProcess it.
        if let Err(err) = child.kill() {
            if !is_ignorable_process_control_error(&err) {
                return Err(to_py_err(err));
            }
        }

        // Drop the writer/master so the background reader thread sees EOF
        // and releases its handle. Otherwise the reader stays blocked
        // forever holding a master clone, which keeps ConPTY alive.
        drop(writer);
        drop(master);

        // Now block until the child is reaped. This is safe to call
        // unbounded because `close()` invoked us inside `py.allow_threads`,
        // so the GIL is released and other Python threads can make
        // progress. After the explicit kill() above, this returns within
        // milliseconds in practice.
        let code = match child.wait() {
            Ok(status) => portable_exit_code(status),
            Err(_) => -9,
        };
        drop(child);
        #[cfg(windows)]
        drop(_job);

        self.store_returncode(code);
        self.mark_reader_closed();
        Ok(())
    }

    /// Best-effort, non-blocking teardown for use from `Drop`.
    ///
    /// `Drop` runs while Python holds the GIL (it is invoked by PyO3 during
    /// finalization), so we cannot call any blocking syscalls here. We kill
    /// the child, drop every handle so the OS reclaims the file descriptors,
    /// and let the OS reap the process. The background reader thread will
    /// notice EOF on its master clone and exit on its own.
    // Preserve a stable Rust frame here in release user dumps.
    #[inline(never)]
    fn close_nonblocking(&self) {
        running_process_core::rp_rust_debug_scope!(
            "running_process_py::NativePtyProcess::close_nonblocking"
        );
        let Ok(mut guard) = self.handles.lock() else {
            return;
        };
        let Some(handles) = guard.take() else {
            self.mark_reader_closed();
            return;
        };
        drop(guard);

        #[cfg(windows)]
        let NativePtyHandles {
            master,
            writer,
            mut child,
            _job,
        } = handles;
        #[cfg(not(windows))]
        let NativePtyHandles {
            master,
            writer,
            mut child,
        } = handles;

        if let Err(err) = child.kill() {
            if !is_ignorable_process_control_error(&err) {
                return;
            }
        }
        // Drop writer + master so the reader thread sees EOF immediately.
        drop(writer);
        drop(master);
        // Do NOT call child.wait() here — that would block the interpreter.
        // Drop on the child closes its OS handle; the process is reaped by
        // the OS once it actually exits.
        drop(child);
        #[cfg(windows)]
        drop(_job);
        self.mark_reader_closed();
    }

    fn start_impl(&self) -> PyResult<()> {
        running_process_core::rp_rust_debug_scope!("running_process_py::NativePtyProcess::start");
        let mut guard = self.handles.lock().expect("pty handles mutex poisoned");
        if guard.is_some() {
            return Err(PyRuntimeError::new_err(
                "Pseudo-terminal process already started",
            ));
        }

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: self.rows,
                cols: self.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(to_py_err)?;

        let mut cmd = command_builder_from_argv(&self.argv);
        if let Some(cwd) = &self.cwd {
            cmd.cwd(cwd);
        }
        if let Some(env) = &self.env {
            cmd.env_clear();
            for (key, value) in env {
                cmd.env(key, value);
            }
        }

        let reader = pair.master.try_clone_reader().map_err(to_py_err)?;
        let writer = pair.master.take_writer().map_err(to_py_err)?;
        let child = pair.slave.spawn_command(cmd).map_err(to_py_err)?;
        #[cfg(windows)]
        let job = public_symbols::rp_py_assign_child_to_windows_kill_on_close_job_public(
            child.as_raw_handle(),
        )?;
        #[cfg(windows)]
        public_symbols::rp_apply_windows_pty_priority_public(child.as_raw_handle(), self.nice)?;
        let shared = Arc::clone(&self.reader);
        thread::spawn(move || public_symbols::rp_spawn_pty_reader_public(reader, shared));

        *guard = Some(NativePtyHandles {
            master: pair.master,
            writer,
            child,
            #[cfg(windows)]
            _job: job,
        });
        Ok(())
    }

    fn respond_to_queries_impl(&self, data: &[u8]) -> PyResult<()> {
        #[cfg(windows)]
        {
            return public_symbols::rp_pty_windows_respond_to_queries_public(self, data);
        }

        #[cfg(unix)]
        {
            pty_platform::respond_to_queries(self, data)
        }
    }

    fn resize_impl(&self, rows: u16, cols: u16) -> PyResult<()> {
        running_process_core::rp_rust_debug_scope!("running_process_py::NativePtyProcess::resize");
        let guard = self.handles.lock().expect("pty handles mutex poisoned");
        if let Some(handles) = guard.as_ref() {
            handles
                .master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(to_py_err)?;
        }
        Ok(())
    }

    fn send_interrupt_impl(&self) -> PyResult<()> {
        running_process_core::rp_rust_debug_scope!(
            "running_process_py::NativePtyProcess::send_interrupt"
        );
        #[cfg(windows)]
        {
            return public_symbols::rp_pty_windows_send_interrupt_public(self);
        }

        #[cfg(unix)]
        {
            pty_platform::send_interrupt(self)
        }
    }

    fn wait_impl(&self, timeout: Option<f64>) -> PyResult<i32> {
        running_process_core::rp_rust_debug_scope!("running_process_py::NativePtyProcess::wait");
        let start = Instant::now();
        loop {
            if let Some(code) = self.poll()? {
                return Ok(code);
            }
            if timeout.is_some_and(|limit| start.elapsed() >= Duration::from_secs_f64(limit)) {
                return Err(PyTimeoutError::new_err("Pseudo-terminal process timed out"));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn terminate_impl(&self) -> PyResult<()> {
        running_process_core::rp_rust_debug_scope!(
            "running_process_py::NativePtyProcess::terminate"
        );
        #[cfg(windows)]
        {
            return public_symbols::rp_pty_windows_terminate_public(self);
        }

        #[cfg(unix)]
        {
            pty_platform::terminate(self)
        }
    }

    fn kill_impl(&self) -> PyResult<()> {
        running_process_core::rp_rust_debug_scope!("running_process_py::NativePtyProcess::kill");
        #[cfg(windows)]
        {
            return public_symbols::rp_pty_windows_kill_public(self);
        }

        #[cfg(unix)]
        {
            pty_platform::kill(self)
        }
    }

    fn terminate_tree_impl(&self) -> PyResult<()> {
        running_process_core::rp_rust_debug_scope!(
            "running_process_py::NativePtyProcess::terminate_tree"
        );
        #[cfg(windows)]
        {
            return public_symbols::rp_pty_windows_terminate_tree_public(self);
        }

        #[cfg(unix)]
        {
            pty_platform::terminate_tree(self)
        }
    }

    fn kill_tree_impl(&self) -> PyResult<()> {
        running_process_core::rp_rust_debug_scope!(
            "running_process_py::NativePtyProcess::kill_tree"
        );
        #[cfg(windows)]
        {
            return public_symbols::rp_pty_windows_kill_tree_public(self);
        }

        #[cfg(unix)]
        {
            pty_platform::kill_tree(self)
        }
    }
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

fn parse_command(command: &Bound<'_, PyAny>, shell: bool) -> PyResult<CommandSpec> {
    if let Ok(command) = command.extract::<String>() {
        if !shell {
            return Err(PyValueError::new_err(
                "String commands require shell=True. Use shell=True or provide command as list[str].",
            ));
        }
        return Ok(CommandSpec::Shell(command));
    }

    if let Ok(command) = command.downcast::<PyList>() {
        let argv = command.extract::<Vec<String>>()?;
        if argv.is_empty() {
            return Err(PyValueError::new_err("command cannot be empty"));
        }
        if shell {
            return Ok(CommandSpec::Shell(argv.join(" ")));
        }
        return Ok(CommandSpec::Argv(argv));
    }

    Err(PyValueError::new_err(
        "command must be either a string or a list[str]",
    ))
}

fn stream_kind(name: &str) -> PyResult<StreamKind> {
    match name {
        "stdout" => Ok(StreamKind::Stdout),
        "stderr" => Ok(StreamKind::Stderr),
        _ => Err(PyValueError::new_err("stream must be 'stdout' or 'stderr'")),
    }
}

fn stdin_mode(name: &str) -> PyResult<StdinMode> {
    match name {
        "inherit" => Ok(StdinMode::Inherit),
        "piped" => Ok(StdinMode::Piped),
        "null" => Ok(StdinMode::Null),
        _ => Err(PyValueError::new_err(
            "stdin_mode must be 'inherit', 'piped', or 'null'",
        )),
    }
}

#[pyclass]
struct NativeRunningProcess {
    inner: NativeProcess,
    text: bool,
    encoding: Option<String>,
    errors: Option<String>,
    #[cfg(windows)]
    creationflags: Option<u32>,
    #[cfg(unix)]
    create_process_group: bool,
}

enum NativeProcessBackend {
    Running(NativeRunningProcess),
    Pty(NativePtyProcess),
}

#[pyclass(name = "NativeProcess")]
struct PyNativeProcess {
    backend: NativeProcessBackend,
}

#[pyclass]
#[derive(Clone)]
struct NativeSignalBool {
    value: Arc<AtomicBool>,
    write_lock: Arc<Mutex<()>>,
}

struct IdleMonitorState {
    last_reset_at: Instant,
    returncode: Option<i32>,
    interrupted: bool,
}

#[pyclass]
struct NativeIdleDetector {
    timeout_seconds: f64,
    stability_window_seconds: f64,
    sample_interval_seconds: f64,
    reset_on_input: bool,
    reset_on_output: bool,
    count_control_churn_as_output: bool,
    enabled: Arc<AtomicBool>,
    state: Mutex<IdleMonitorState>,
    condvar: Condvar,
}

struct PtyBufferState {
    chunks: VecDeque<Vec<u8>>,
    history: Vec<u8>,
    history_bytes: usize,
    closed: bool,
}

#[pyclass]
struct NativePtyBuffer {
    text: bool,
    encoding: String,
    errors: String,
    state: Mutex<PtyBufferState>,
    condvar: Condvar,
}

#[pymethods]
impl NativeRunningProcess {
    #[new]
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (command, cwd=None, shell=false, capture=true, env=None, creationflags=None, text=true, encoding=None, errors=None, stdin_mode_name="inherit", nice=None, create_process_group=false))]
    fn new(
        command: &Bound<'_, PyAny>,
        cwd: Option<String>,
        shell: bool,
        capture: bool,
        env: Option<Bound<'_, PyDict>>,
        creationflags: Option<u32>,
        text: bool,
        encoding: Option<String>,
        errors: Option<String>,
        stdin_mode_name: &str,
        nice: Option<i32>,
        create_process_group: bool,
    ) -> PyResult<Self> {
        let parsed = parse_command(command, shell)?;
        let env_pairs = env
            .map(|mapping| {
                mapping
                    .iter()
                    .map(|(key, value)| Ok((key.extract::<String>()?, value.extract::<String>()?)))
                    .collect::<PyResult<Vec<(String, String)>>>()
            })
            .transpose()?;

        Ok(Self {
            inner: NativeProcess::new(ProcessConfig {
                command: parsed,
                cwd: cwd.map(PathBuf::from),
                env: env_pairs,
                capture,
                creationflags,
                create_process_group,
                stdin_mode: stdin_mode(stdin_mode_name)?,
                nice,
            }),
            text,
            encoding,
            errors,
            #[cfg(windows)]
            creationflags,
            #[cfg(unix)]
            create_process_group,
        })
    }

    #[inline(never)]
    fn start(&self) -> PyResult<()> {
        public_symbols::rp_native_running_process_start_public(self)
    }

    fn poll(&self) -> PyResult<Option<i32>> {
        self.inner.poll().map_err(to_py_err)
    }

    #[pyo3(signature = (timeout=None))]
    #[inline(never)]
    fn wait(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<i32> {
        public_symbols::rp_native_running_process_wait_public(self, py, timeout)
    }

    #[inline(never)]
    fn kill(&self) -> PyResult<()> {
        public_symbols::rp_native_running_process_kill_public(self)
    }

    #[inline(never)]
    fn terminate(&self) -> PyResult<()> {
        public_symbols::rp_native_running_process_terminate_public(self)
    }

    #[inline(never)]
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        public_symbols::rp_native_running_process_close_public(self, py)
    }

    fn terminate_group(&self) -> PyResult<()> {
        #[cfg(unix)]
        {
            let pid = self
                .inner
                .pid()
                .ok_or_else(|| PyRuntimeError::new_err("process is not running"))?;
            if self.create_process_group {
                unix_signal_process_group(pid as i32, UnixSignal::Terminate).map_err(to_py_err)?;
                return Ok(());
            }
        }
        self.inner.terminate().map_err(to_py_err)
    }

    fn write_stdin(&self, data: &[u8]) -> PyResult<()> {
        self.inner.write_stdin(data).map_err(to_py_err)
    }

    #[getter]
    fn pid(&self) -> Option<u32> {
        self.inner.pid()
    }

    #[getter]
    fn returncode(&self) -> Option<i32> {
        self.inner.returncode()
    }

    #[inline(never)]
    fn send_interrupt(&self) -> PyResult<()> {
        public_symbols::rp_native_running_process_send_interrupt_public(self)
    }

    fn kill_group(&self) -> PyResult<()> {
        #[cfg(unix)]
        {
            let pid = self
                .inner
                .pid()
                .ok_or_else(|| PyRuntimeError::new_err("process is not running"))?;
            if self.create_process_group {
                unix_signal_process_group(pid as i32, UnixSignal::Kill).map_err(to_py_err)?;
                return Ok(());
            }
        }
        self.inner.kill().map_err(to_py_err)
    }

    fn has_pending_combined(&self) -> bool {
        self.inner.has_pending_combined()
    }

    fn has_pending_stream(&self, stream: &str) -> PyResult<bool> {
        Ok(self.inner.has_pending_stream(stream_kind(stream)?))
    }

    fn drain_combined(&self, py: Python<'_>) -> PyResult<Vec<(String, Py<PyAny>)>> {
        self.inner
            .drain_combined()
            .into_iter()
            .map(|event| {
                Ok((
                    event.stream.as_str().to_string(),
                    self.decode_line(py, &event.line)?,
                ))
            })
            .collect()
    }

    fn drain_stream(&self, py: Python<'_>, stream: &str) -> PyResult<Vec<Py<PyAny>>> {
        self.inner
            .drain_stream(stream_kind(stream)?)
            .into_iter()
            .map(|line| self.decode_line(py, &line))
            .collect()
    }

    #[pyo3(signature = (timeout=None))]
    fn take_combined_line(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> PyResult<(String, Option<String>, Option<Py<PyAny>>)> {
        match self
            .inner
            .read_combined(timeout.map(Duration::from_secs_f64))
        {
            ReadStatus::Line(StreamEvent { stream, line }) => Ok((
                "line".into(),
                Some(stream.as_str().into()),
                Some(self.decode_line(py, &line)?),
            )),
            ReadStatus::Timeout => Ok(("timeout".into(), None, None)),
            ReadStatus::Eof => Ok(("eof".into(), None, None)),
        }
    }

    #[pyo3(signature = (stream, timeout=None))]
    fn take_stream_line(
        &self,
        py: Python<'_>,
        stream: &str,
        timeout: Option<f64>,
    ) -> PyResult<(String, Option<Py<PyAny>>)> {
        match self
            .inner
            .read_stream(stream_kind(stream)?, timeout.map(Duration::from_secs_f64))
        {
            ReadStatus::Line(line) => Ok(("line".into(), Some(self.decode_line(py, &line)?))),
            ReadStatus::Timeout => Ok(("timeout".into(), None)),
            ReadStatus::Eof => Ok(("eof".into(), None)),
        }
    }

    fn captured_stdout(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        self.inner
            .captured_stdout()
            .into_iter()
            .map(|line| self.decode_line(py, &line))
            .collect()
    }

    fn captured_stderr(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        self.inner
            .captured_stderr()
            .into_iter()
            .map(|line| self.decode_line(py, &line))
            .collect()
    }

    fn captured_combined(&self, py: Python<'_>) -> PyResult<Vec<(String, Py<PyAny>)>> {
        self.inner
            .captured_combined()
            .into_iter()
            .map(|event| {
                Ok((
                    event.stream.as_str().to_string(),
                    self.decode_line(py, &event.line)?,
                ))
            })
            .collect()
    }

    fn captured_stream_bytes(&self, stream: &str) -> PyResult<usize> {
        Ok(self.inner.captured_stream_bytes(stream_kind(stream)?))
    }

    fn captured_combined_bytes(&self) -> usize {
        self.inner.captured_combined_bytes()
    }

    fn clear_captured_stream(&self, stream: &str) -> PyResult<usize> {
        Ok(self.inner.clear_captured_stream(stream_kind(stream)?))
    }

    fn clear_captured_combined(&self) -> usize {
        self.inner.clear_captured_combined()
    }

    #[pyo3(signature = (stream, pattern, is_regex=false, timeout=None))]
    fn expect(
        &self,
        py: Python<'_>,
        stream: &str,
        pattern: &str,
        is_regex: bool,
        timeout: Option<f64>,
    ) -> PyResult<ExpectResult> {
        let stream_kind = if stream == "combined" {
            None
        } else {
            Some(stream_kind(stream)?)
        };
        let mut buffer = match stream_kind {
            Some(kind) => self.captured_stream_text(py, kind)?,
            None => self.captured_combined_text(py)?,
        };
        let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs_f64(secs));

        loop {
            if let Some((matched, start, end, groups)) =
                self.find_expect_match(&buffer, pattern, is_regex)?
            {
                return Ok((
                    "match".to_string(),
                    buffer,
                    Some(matched),
                    Some(start),
                    Some(end),
                    groups,
                ));
            }

            let wait_timeout = deadline.map(|limit| {
                let now = Instant::now();
                if now >= limit {
                    Duration::from_secs(0)
                } else {
                    limit
                        .saturating_duration_since(now)
                        .min(Duration::from_millis(100))
                }
            });
            if deadline.is_some_and(|limit| Instant::now() >= limit) {
                return Ok(("timeout".to_string(), buffer, None, None, None, Vec::new()));
            }

            match self.read_status_text(stream_kind, wait_timeout)? {
                ReadStatus::Line(line) => {
                    let decoded = self.decode_line_to_string(py, &line)?;
                    buffer.push_str(&decoded);
                    buffer.push('\n');
                }
                ReadStatus::Timeout => {
                    // Keep polling until the overall expect deadline expires.
                    continue;
                }
                ReadStatus::Eof => {
                    return Ok(("eof".to_string(), buffer, None, None, None, Vec::new()));
                }
            }
        }
    }

    #[staticmethod]
    fn is_pty_available() -> bool {
        false
    }
}

#[pymethods]
impl PyNativeProcess {
    #[new]
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (command, cwd=None, shell=false, capture=true, env=None, creationflags=None, text=true, encoding=None, errors=None, stdin_mode_name="inherit", nice=None, create_process_group=false))]
    fn new(
        command: &Bound<'_, PyAny>,
        cwd: Option<String>,
        shell: bool,
        capture: bool,
        env: Option<Bound<'_, PyDict>>,
        creationflags: Option<u32>,
        text: bool,
        encoding: Option<String>,
        errors: Option<String>,
        stdin_mode_name: &str,
        nice: Option<i32>,
        create_process_group: bool,
    ) -> PyResult<Self> {
        Ok(Self {
            backend: NativeProcessBackend::Running(NativeRunningProcess::new(
                command,
                cwd,
                shell,
                capture,
                env,
                creationflags,
                text,
                encoding,
                errors,
                stdin_mode_name,
                nice,
                create_process_group,
            )?),
        })
    }

    #[staticmethod]
    #[pyo3(signature = (argv, cwd=None, env=None, rows=24, cols=80, nice=None))]
    fn for_pty(
        argv: Vec<String>,
        cwd: Option<String>,
        env: Option<Bound<'_, PyDict>>,
        rows: u16,
        cols: u16,
        nice: Option<i32>,
    ) -> PyResult<Self> {
        Ok(Self {
            backend: NativeProcessBackend::Pty(NativePtyProcess::new(
                argv, cwd, env, rows, cols, nice,
            )?),
        })
    }

    fn start(&self) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.start(),
            NativeProcessBackend::Pty(process) => process.start(),
        }
    }

    fn poll(&self) -> PyResult<Option<i32>> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.poll(),
            NativeProcessBackend::Pty(process) => process.poll(),
        }
    }

    #[pyo3(signature = (timeout=None))]
    fn wait(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<i32> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.wait(py, timeout),
            NativeProcessBackend::Pty(process) => py.allow_threads(|| process.wait(timeout)),
        }
    }

    fn kill(&self) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.kill(),
            NativeProcessBackend::Pty(process) => process.kill(),
        }
    }

    fn terminate(&self) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.terminate(),
            NativeProcessBackend::Pty(process) => process.terminate(),
        }
    }

    fn terminate_group(&self) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.terminate_group(),
            NativeProcessBackend::Pty(process) => process.terminate_tree(),
        }
    }

    fn kill_group(&self) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.kill_group(),
            NativeProcessBackend::Pty(process) => process.kill_tree(),
        }
    }

    fn has_pending_combined(&self) -> PyResult<bool> {
        match &self.backend {
            NativeProcessBackend::Running(process) => Ok(process.has_pending_combined()),
            NativeProcessBackend::Pty(_) => Ok(false),
        }
    }

    fn has_pending_stream(&self, stream: &str) -> PyResult<bool> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.has_pending_stream(stream),
            NativeProcessBackend::Pty(_) => Ok(false),
        }
    }

    fn drain_combined(&self, py: Python<'_>) -> PyResult<Vec<(String, Py<PyAny>)>> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.drain_combined(py),
            NativeProcessBackend::Pty(_) => Ok(Vec::new()),
        }
    }

    fn drain_stream(&self, py: Python<'_>, stream: &str) -> PyResult<Vec<Py<PyAny>>> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.drain_stream(py, stream),
            NativeProcessBackend::Pty(_) => {
                let _ = stream;
                Ok(Vec::new())
            }
        }
    }

    #[pyo3(signature = (timeout=None))]
    fn take_combined_line(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> PyResult<(String, Option<String>, Option<Py<PyAny>>)> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.take_combined_line(py, timeout),
            NativeProcessBackend::Pty(_) => Ok(("eof".into(), None, None)),
        }
    }

    #[pyo3(signature = (stream, timeout=None))]
    fn take_stream_line(
        &self,
        py: Python<'_>,
        stream: &str,
        timeout: Option<f64>,
    ) -> PyResult<(String, Option<Py<PyAny>>)> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.take_stream_line(py, stream, timeout),
            NativeProcessBackend::Pty(_) => {
                let _ = (py, stream, timeout);
                Ok(("eof".into(), None))
            }
        }
    }

    fn captured_stdout(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.captured_stdout(py),
            NativeProcessBackend::Pty(_) => Ok(Vec::new()),
        }
    }

    fn captured_stderr(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.captured_stderr(py),
            NativeProcessBackend::Pty(_) => Ok(Vec::new()),
        }
    }

    fn captured_combined(&self, py: Python<'_>) -> PyResult<Vec<(String, Py<PyAny>)>> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.captured_combined(py),
            NativeProcessBackend::Pty(_) => Ok(Vec::new()),
        }
    }

    fn captured_stream_bytes(&self, stream: &str) -> PyResult<usize> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.captured_stream_bytes(stream),
            NativeProcessBackend::Pty(_) => Ok(0),
        }
    }

    fn captured_combined_bytes(&self) -> PyResult<usize> {
        match &self.backend {
            NativeProcessBackend::Running(process) => Ok(process.captured_combined_bytes()),
            NativeProcessBackend::Pty(_) => Ok(0),
        }
    }

    fn clear_captured_stream(&self, stream: &str) -> PyResult<usize> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.clear_captured_stream(stream),
            NativeProcessBackend::Pty(_) => Ok(0),
        }
    }

    fn clear_captured_combined(&self) -> PyResult<usize> {
        match &self.backend {
            NativeProcessBackend::Running(process) => Ok(process.clear_captured_combined()),
            NativeProcessBackend::Pty(_) => Ok(0),
        }
    }

    fn write_stdin(&self, data: &[u8]) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.write_stdin(data),
            NativeProcessBackend::Pty(process) => process.write(data),
        }
    }

    fn write(&self, data: &[u8]) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.write_stdin(data),
            NativeProcessBackend::Pty(process) => process.write(data),
        }
    }

    #[pyo3(signature = (timeout=None))]
    fn read_chunk(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<Py<PyAny>> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => process.read_chunk(py, timeout),
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "read_chunk is only available for PTY-backed NativeProcess",
            )),
        }
    }

    #[pyo3(signature = (timeout=None))]
    fn wait_for_pty_reader_closed(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<bool> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => process.wait_for_reader_closed(py, timeout),
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "wait_for_pty_reader_closed is only available for PTY-backed NativeProcess",
            )),
        }
    }

    fn respond_to_queries(&self, data: &[u8]) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => process.respond_to_queries(data),
            NativeProcessBackend::Running(_) => Ok(()),
        }
    }

    fn resize(&self, rows: u16, cols: u16) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => process.resize(rows, cols),
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "resize is only available for PTY-backed NativeProcess",
            )),
        }
    }

    fn send_interrupt(&self) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.send_interrupt(),
            NativeProcessBackend::Pty(process) => process.send_interrupt(),
        }
    }

    #[pyo3(signature = (stream, pattern, is_regex=false, timeout=None))]
    fn expect(
        &self,
        py: Python<'_>,
        stream: &str,
        pattern: &str,
        is_regex: bool,
        timeout: Option<f64>,
    ) -> PyResult<ExpectResult> {
        match &self.backend {
            NativeProcessBackend::Running(process) => {
                process.expect(py, stream, pattern, is_regex, timeout)
            }
            NativeProcessBackend::Pty(_) => Err(PyRuntimeError::new_err(
                "expect is only available for subprocess-backed NativeProcess",
            )),
        }
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.close(py),
            NativeProcessBackend::Pty(process) => process.close(py),
        }
    }

    #[getter]
    fn pid(&self) -> PyResult<Option<u32>> {
        match &self.backend {
            NativeProcessBackend::Running(process) => Ok(process.pid()),
            NativeProcessBackend::Pty(process) => process.pid(),
        }
    }

    #[getter]
    fn returncode(&self) -> PyResult<Option<i32>> {
        match &self.backend {
            NativeProcessBackend::Running(process) => Ok(process.returncode()),
            NativeProcessBackend::Pty(process) => Ok(*process
                .returncode
                .lock()
                .expect("pty returncode mutex poisoned")),
        }
    }

    fn is_pty(&self) -> bool {
        matches!(self.backend, NativeProcessBackend::Pty(_))
    }
}

#[pymethods]
impl NativePtyProcess {
    #[new]
    #[pyo3(signature = (argv, cwd=None, env=None, rows=24, cols=80, nice=None))]
    fn new(
        argv: Vec<String>,
        cwd: Option<String>,
        env: Option<Bound<'_, PyDict>>,
        rows: u16,
        cols: u16,
        nice: Option<i32>,
    ) -> PyResult<Self> {
        if argv.is_empty() {
            return Err(PyValueError::new_err("command cannot be empty"));
        }
        #[cfg(not(windows))]
        let _ = nice;
        let env_pairs = env
            .map(|mapping| {
                mapping
                    .iter()
                    .map(|(key, value)| Ok((key.extract::<String>()?, value.extract::<String>()?)))
                    .collect::<PyResult<Vec<(String, String)>>>()
            })
            .transpose()?;
        Ok(Self {
            argv,
            cwd,
            env: env_pairs,
            rows,
            cols,
            #[cfg(windows)]
            nice,
            handles: Mutex::new(None),
            reader: Arc::new(PtyReadShared {
                state: Mutex::new(PtyReadState {
                    chunks: VecDeque::new(),
                    closed: false,
                }),
                condvar: Condvar::new(),
            }),
            returncode: Mutex::new(None),
        })
    }

    #[inline(never)]
    fn start(&self) -> PyResult<()> {
        public_symbols::rp_native_pty_process_start_public(self)
    }

    #[pyo3(signature = (timeout=None))]
    fn read_chunk(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<Py<PyAny>> {
        // Wait for a chunk WITHOUT holding the GIL. The previous version
        // called `condvar.wait()` while still holding the GIL, which starved
        // every other Python thread for the duration of the wait. With a
        // 100ms read poll loop, that meant the main thread could only run
        // for a few microseconds every 100ms — turning ordinary calls like
        // `os.path.realpath` into ~430ms operations and producing apparent
        // deadlocks during pytest failure formatting.
        enum WaitOutcome {
            Chunk(Vec<u8>),
            Closed,
            Timeout,
        }

        let outcome = py.allow_threads(|| -> WaitOutcome {
            let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs_f64(secs));
            let mut guard = self.reader.state.lock().expect("pty read mutex poisoned");
            loop {
                if let Some(chunk) = guard.chunks.pop_front() {
                    return WaitOutcome::Chunk(chunk);
                }
                if guard.closed {
                    return WaitOutcome::Closed;
                }
                match deadline {
                    Some(deadline) => {
                        let now = Instant::now();
                        if now >= deadline {
                            return WaitOutcome::Timeout;
                        }
                        let wait = deadline.saturating_duration_since(now);
                        let result = self
                            .reader
                            .condvar
                            .wait_timeout(guard, wait)
                            .expect("pty read mutex poisoned");
                        guard = result.0;
                    }
                    None => {
                        guard = self
                            .reader
                            .condvar
                            .wait(guard)
                            .expect("pty read mutex poisoned");
                    }
                }
            }
        });

        match outcome {
            WaitOutcome::Chunk(chunk) => Ok(PyBytes::new(py, &chunk).into_any().unbind()),
            WaitOutcome::Closed => Err(PyRuntimeError::new_err("Pseudo-terminal stream is closed")),
            WaitOutcome::Timeout => Err(PyTimeoutError::new_err(
                "No pseudo-terminal output available before timeout",
            )),
        }
    }

    #[pyo3(signature = (timeout=None))]
    fn wait_for_reader_closed(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<bool> {
        let closed = py.allow_threads(|| {
            let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs_f64(secs));
            let mut guard = self.reader.state.lock().expect("pty read mutex poisoned");
            loop {
                if guard.closed {
                    return true;
                }
                match deadline {
                    Some(deadline) => {
                        let now = Instant::now();
                        if now >= deadline {
                            return false;
                        }
                        let wait = deadline.saturating_duration_since(now);
                        let result = self
                            .reader
                            .condvar
                            .wait_timeout(guard, wait)
                            .expect("pty read mutex poisoned");
                        guard = result.0;
                    }
                    None => {
                        guard = self
                            .reader
                            .condvar
                            .wait(guard)
                            .expect("pty read mutex poisoned");
                    }
                }
            }
        });
        Ok(closed)
    }

    fn write(&self, data: &[u8]) -> PyResult<()> {
        let mut guard = self.handles.lock().expect("pty handles mutex poisoned");
        let handles = guard
            .as_mut()
            .ok_or_else(|| PyRuntimeError::new_err("Pseudo-terminal process is not running"))?;
        #[cfg(windows)]
        let payload = public_symbols::rp_pty_windows_input_payload_public(data);
        #[cfg(unix)]
        let payload = pty_platform::input_payload(data);
        handles.writer.write_all(&payload).map_err(to_py_err)?;
        handles.writer.flush().map_err(to_py_err)
    }

    fn respond_to_queries(&self, data: &[u8]) -> PyResult<()> {
        public_symbols::rp_native_pty_process_respond_to_queries_public(self, data)
    }

    #[inline(never)]
    fn resize(&self, rows: u16, cols: u16) -> PyResult<()> {
        public_symbols::rp_native_pty_process_resize_public(self, rows, cols)
    }

    #[inline(never)]
    fn send_interrupt(&self) -> PyResult<()> {
        public_symbols::rp_native_pty_process_send_interrupt_public(self)
    }

    fn poll(&self) -> PyResult<Option<i32>> {
        let mut guard = self.handles.lock().expect("pty handles mutex poisoned");
        let Some(handles) = guard.as_mut() else {
            return Ok(*self
                .returncode
                .lock()
                .expect("pty returncode mutex poisoned"));
        };
        let status = handles.child.try_wait().map_err(to_py_err)?;
        let code = status.map(portable_exit_code);
        if code.is_some() {
            self.store_returncode(code.expect("checked is_some"));
        }
        Ok(code)
    }

    #[pyo3(signature = (timeout=None))]
    #[inline(never)]
    fn wait(&self, timeout: Option<f64>) -> PyResult<i32> {
        public_symbols::rp_native_pty_process_wait_public(self, timeout)
    }

    #[inline(never)]
    fn terminate(&self) -> PyResult<()> {
        public_symbols::rp_native_pty_process_terminate_public(self)
    }

    #[inline(never)]
    fn kill(&self) -> PyResult<()> {
        public_symbols::rp_native_pty_process_kill_public(self)
    }

    #[inline(never)]
    fn terminate_tree(&self) -> PyResult<()> {
        public_symbols::rp_native_pty_process_terminate_tree_public(self)
    }

    #[inline(never)]
    fn kill_tree(&self) -> PyResult<()> {
        public_symbols::rp_native_pty_process_kill_tree_public(self)
    }

    #[getter]
    fn pid(&self) -> PyResult<Option<u32>> {
        let mut guard = self.handles.lock().expect("pty handles mutex poisoned");
        if let Some(handles) = guard.as_mut() {
            return Ok(handles.child.process_id());
        }
        Ok(None)
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        // Release the GIL while waiting on the child — otherwise the wait
        // blocks every other Python thread (including the one that may need
        // to drop additional references for the child to actually exit).
        public_symbols::rp_native_pty_process_close_public(self, py)
    }
}

impl Drop for NativePtyProcess {
    fn drop(&mut self) {
        // Drop runs under the GIL during PyO3 finalization. Calling
        // `close_impl` here would block the interpreter on `child.wait()`
        // and deadlock with the background reader thread. Use the
        // non-blocking teardown instead.
        public_symbols::rp_native_pty_process_close_nonblocking_public(self);
    }
}

#[pymethods]
impl NativeSignalBool {
    #[new]
    #[pyo3(signature = (value=false))]
    fn new(value: bool) -> Self {
        Self {
            value: Arc::new(AtomicBool::new(value)),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    #[getter]
    fn value(&self) -> bool {
        self.load_nolock()
    }

    #[setter]
    fn set_value(&self, value: bool) {
        self.store_locked(value);
    }

    fn load_nolock(&self) -> bool {
        self.value.load(Ordering::Acquire)
    }

    fn store_locked(&self, value: bool) {
        let _guard = self.write_lock.lock().expect("signal bool mutex poisoned");
        self.value.store(value, Ordering::Release);
    }

    fn compare_and_swap_locked(&self, current: bool, new: bool) -> bool {
        let _guard = self.write_lock.lock().expect("signal bool mutex poisoned");
        self.value
            .compare_exchange(current, new, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

#[pymethods]
impl NativePtyBuffer {
    #[new]
    #[pyo3(signature = (text=false, encoding="utf-8", errors="replace"))]
    fn new(text: bool, encoding: &str, errors: &str) -> Self {
        Self {
            text,
            encoding: encoding.to_string(),
            errors: errors.to_string(),
            state: Mutex::new(PtyBufferState {
                chunks: VecDeque::new(),
                history: Vec::new(),
                history_bytes: 0,
                closed: false,
            }),
            condvar: Condvar::new(),
        }
    }

    fn available(&self) -> bool {
        !self
            .state
            .lock()
            .expect("pty buffer mutex poisoned")
            .chunks
            .is_empty()
    }

    fn record_output(&self, data: &[u8]) {
        let mut guard = self.state.lock().expect("pty buffer mutex poisoned");
        guard.history_bytes += data.len();
        guard.history.extend_from_slice(data);
        guard.chunks.push_back(data.to_vec());
        self.condvar.notify_all();
    }

    fn close(&self) {
        let mut guard = self.state.lock().expect("pty buffer mutex poisoned");
        guard.closed = true;
        self.condvar.notify_all();
    }

    #[pyo3(signature = (timeout=None))]
    fn read(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<Py<PyAny>> {
        // Mirror NativePtyProcess::read_chunk: do the wait WITHOUT the GIL
        // so other Python threads (notably the test/main thread) can make
        // progress instead of being starved by our 100ms read poll loop.
        enum WaitOutcome {
            Chunk(Vec<u8>),
            Closed,
            Timeout,
        }

        let outcome = py.allow_threads(|| -> WaitOutcome {
            let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs_f64(secs));
            let mut guard = self.state.lock().expect("pty buffer mutex poisoned");
            loop {
                if let Some(chunk) = guard.chunks.pop_front() {
                    return WaitOutcome::Chunk(chunk);
                }
                if guard.closed {
                    return WaitOutcome::Closed;
                }
                match deadline {
                    Some(deadline) => {
                        let now = Instant::now();
                        if now >= deadline {
                            return WaitOutcome::Timeout;
                        }
                        let wait = deadline.saturating_duration_since(now);
                        let result = self
                            .condvar
                            .wait_timeout(guard, wait)
                            .expect("pty buffer mutex poisoned");
                        guard = result.0;
                    }
                    None => {
                        guard = self.condvar.wait(guard).expect("pty buffer mutex poisoned");
                    }
                }
            }
        });

        match outcome {
            WaitOutcome::Chunk(chunk) => self.decode_chunk(py, &chunk),
            WaitOutcome::Closed => Err(PyRuntimeError::new_err("Pseudo-terminal stream is closed")),
            WaitOutcome::Timeout => Err(PyTimeoutError::new_err(
                "No pseudo-terminal output available before timeout",
            )),
        }
    }

    fn read_non_blocking(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        let mut guard = self.state.lock().expect("pty buffer mutex poisoned");
        if let Some(chunk) = guard.chunks.pop_front() {
            return self.decode_chunk(py, &chunk).map(Some);
        }
        if guard.closed {
            return Err(PyRuntimeError::new_err("Pseudo-terminal stream is closed"));
        }
        Ok(None)
    }

    fn drain(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        let mut guard = self.state.lock().expect("pty buffer mutex poisoned");
        guard
            .chunks
            .drain(..)
            .map(|chunk| self.decode_chunk(py, &chunk))
            .collect()
    }

    fn output(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let guard = self.state.lock().expect("pty buffer mutex poisoned");
        self.decode_chunk(py, &guard.history)
    }

    fn output_since(&self, py: Python<'_>, start: usize) -> PyResult<Py<PyAny>> {
        let guard = self.state.lock().expect("pty buffer mutex poisoned");
        let start = start.min(guard.history.len());
        self.decode_chunk(py, &guard.history[start..])
    }

    fn history_bytes(&self) -> usize {
        self.state
            .lock()
            .expect("pty buffer mutex poisoned")
            .history_bytes
    }

    fn clear_history(&self) -> usize {
        let mut guard = self.state.lock().expect("pty buffer mutex poisoned");
        let released = guard.history_bytes;
        guard.history.clear();
        guard.history_bytes = 0;
        released
    }
}

impl NativeRunningProcess {
    fn start_impl(&self) -> PyResult<()> {
        running_process_core::rp_rust_debug_scope!(
            "running_process_py::NativeRunningProcess::start"
        );
        self.inner.start().map_err(to_py_err)
    }

    fn wait_impl(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<i32> {
        running_process_core::rp_rust_debug_scope!(
            "running_process_py::NativeRunningProcess::wait"
        );
        py.allow_threads(|| {
            self.inner
                .wait(timeout.map(Duration::from_secs_f64))
                .map_err(process_err_to_py)
        })
    }

    fn kill_impl(&self) -> PyResult<()> {
        running_process_core::rp_rust_debug_scope!(
            "running_process_py::NativeRunningProcess::kill"
        );
        self.inner.kill().map_err(to_py_err)
    }

    fn terminate_impl(&self) -> PyResult<()> {
        running_process_core::rp_rust_debug_scope!(
            "running_process_py::NativeRunningProcess::terminate"
        );
        self.inner.terminate().map_err(to_py_err)
    }

    fn close_impl(&self, py: Python<'_>) -> PyResult<()> {
        running_process_core::rp_rust_debug_scope!(
            "running_process_py::NativeRunningProcess::close"
        );
        py.allow_threads(|| self.inner.close().map_err(process_err_to_py))
    }

    fn send_interrupt_impl(&self) -> PyResult<()> {
        running_process_core::rp_rust_debug_scope!(
            "running_process_py::NativeRunningProcess::send_interrupt"
        );
        let pid = self
            .inner
            .pid()
            .ok_or_else(|| PyRuntimeError::new_err("process is not running"))?;

        #[cfg(windows)]
        {
            return public_symbols::rp_windows_generate_console_ctrl_break_public(
                pid,
                self.creationflags,
            );
        }

        #[cfg(unix)]
        {
            if self.create_process_group {
                unix_signal_process_group(pid as i32, UnixSignal::Interrupt).map_err(to_py_err)?;
            } else {
                unix_signal_process(pid, UnixSignal::Interrupt).map_err(to_py_err)?;
            }
            Ok(())
        }
    }

    fn decode_line_to_string(&self, py: Python<'_>, line: &[u8]) -> PyResult<String> {
        if !self.text {
            return Ok(String::from_utf8_lossy(line).into_owned());
        }
        PyBytes::new(py, line)
            .call_method1(
                "decode",
                (
                    self.encoding.as_deref().unwrap_or("utf-8"),
                    self.errors.as_deref().unwrap_or("replace"),
                ),
            )?
            .extract()
    }

    fn captured_stream_text(&self, py: Python<'_>, stream: StreamKind) -> PyResult<String> {
        let lines = match stream {
            StreamKind::Stdout => self.inner.captured_stdout(),
            StreamKind::Stderr => self.inner.captured_stderr(),
        };
        let mut text = String::new();
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                text.push('\n');
            }
            text.push_str(&self.decode_line_to_string(py, line)?);
        }
        Ok(text)
    }

    fn captured_combined_text(&self, py: Python<'_>) -> PyResult<String> {
        let lines = self.inner.captured_combined();
        let mut text = String::new();
        for (index, event) in lines.iter().enumerate() {
            if index > 0 {
                text.push('\n');
            }
            text.push_str(&self.decode_line_to_string(py, &event.line)?);
        }
        Ok(text)
    }

    fn read_status_text(
        &self,
        stream: Option<StreamKind>,
        timeout: Option<Duration>,
    ) -> PyResult<ReadStatus<Vec<u8>>> {
        Ok(match stream {
            Some(kind) => self.inner.read_stream(kind, timeout),
            None => match self.inner.read_combined(timeout) {
                ReadStatus::Line(StreamEvent { line, .. }) => ReadStatus::Line(line),
                ReadStatus::Timeout => ReadStatus::Timeout,
                ReadStatus::Eof => ReadStatus::Eof,
            },
        })
    }

    fn find_expect_match(
        &self,
        buffer: &str,
        pattern: &str,
        is_regex: bool,
    ) -> PyResult<Option<ExpectDetails>> {
        if !is_regex {
            let Some(start) = buffer.find(pattern) else {
                return Ok(None);
            };
            return Ok(Some((
                pattern.to_string(),
                start,
                start + pattern.len(),
                Vec::new(),
            )));
        }

        let regex = Regex::new(pattern).map_err(to_py_err)?;
        let Some(captures) = regex.captures(buffer) else {
            return Ok(None);
        };
        let whole = captures
            .get(0)
            .ok_or_else(|| PyRuntimeError::new_err("regex capture missing group 0"))?;
        let groups = captures
            .iter()
            .skip(1)
            .map(|group| {
                group
                    .map(|value| value.as_str().to_string())
                    .unwrap_or_default()
            })
            .collect();
        Ok(Some((
            whole.as_str().to_string(),
            whole.start(),
            whole.end(),
            groups,
        )))
    }

    fn decode_line(&self, py: Python<'_>, line: &[u8]) -> PyResult<Py<PyAny>> {
        if !self.text {
            return Ok(PyBytes::new(py, line).into_any().unbind());
        }
        Ok(PyBytes::new(py, line)
            .call_method1(
                "decode",
                (
                    self.encoding.as_deref().unwrap_or("utf-8"),
                    self.errors.as_deref().unwrap_or("replace"),
                ),
            )?
            .into_any()
            .unbind())
    }
}

impl NativePtyBuffer {
    fn decode_chunk(&self, py: Python<'_>, line: &[u8]) -> PyResult<Py<PyAny>> {
        if !self.text {
            return Ok(PyBytes::new(py, line).into_any().unbind());
        }
        Ok(PyBytes::new(py, line)
            .call_method1("decode", (&self.encoding, &self.errors))?
            .into_any()
            .unbind())
    }
}

#[pymethods]
impl NativeIdleDetector {
    #[new]
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (timeout_seconds, stability_window_seconds, sample_interval_seconds, enabled_signal, reset_on_input=true, reset_on_output=true, count_control_churn_as_output=true, initial_idle_for_seconds=0.0))]
    fn new(
        py: Python<'_>,
        timeout_seconds: f64,
        stability_window_seconds: f64,
        sample_interval_seconds: f64,
        enabled_signal: Py<NativeSignalBool>,
        reset_on_input: bool,
        reset_on_output: bool,
        count_control_churn_as_output: bool,
        initial_idle_for_seconds: f64,
    ) -> Self {
        let now = Instant::now();
        let initial_idle_for_seconds = initial_idle_for_seconds.max(0.0);
        let last_reset_at = now
            .checked_sub(Duration::from_secs_f64(initial_idle_for_seconds))
            .unwrap_or(now);
        let enabled = enabled_signal.borrow(py).value.clone();
        Self {
            timeout_seconds,
            stability_window_seconds,
            sample_interval_seconds,
            reset_on_input,
            reset_on_output,
            count_control_churn_as_output,
            enabled,
            state: Mutex::new(IdleMonitorState {
                last_reset_at,
                returncode: None,
                interrupted: false,
            }),
            condvar: Condvar::new(),
        }
    }

    #[getter]
    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    #[setter]
    fn set_enabled(&self, enabled: bool) {
        let was_enabled = self.enabled.swap(enabled, Ordering::AcqRel);
        if enabled && !was_enabled {
            let mut guard = self.state.lock().expect("idle monitor mutex poisoned");
            guard.last_reset_at = Instant::now();
        }
        self.condvar.notify_all();
    }

    fn record_input(&self, byte_count: usize) {
        if !self.reset_on_input || byte_count == 0 {
            return;
        }
        let mut guard = self.state.lock().expect("idle monitor mutex poisoned");
        guard.last_reset_at = Instant::now();
        self.condvar.notify_all();
    }

    fn record_output(&self, data: &[u8]) {
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

    fn mark_exit(&self, returncode: i32, interrupted: bool) {
        let mut guard = self.state.lock().expect("idle monitor mutex poisoned");
        guard.returncode = Some(returncode);
        guard.interrupted = interrupted;
        self.condvar.notify_all();
    }

    #[pyo3(signature = (timeout=None))]
    fn wait(&self, py: Python<'_>, timeout: Option<f64>) -> (bool, String, f64, Option<i32>) {
        py.allow_threads(|| {
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
        })
    }
}

fn control_churn_bytes(data: &[u8]) -> usize {
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

fn command_builder_from_argv(argv: &[String]) -> CommandBuilder {
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
fn spawn_pty_reader(mut reader: Box<dyn Read + Send>, shared: Arc<PtyReadShared>) {
    running_process_core::rp_rust_debug_scope!("running_process_py::spawn_pty_reader");
    let mut chunk = [0_u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let mut guard = shared.state.lock().expect("pty read mutex poisoned");
                guard.chunks.push_back(chunk[..n].to_vec());
                shared.condvar.notify_all();
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
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

fn portable_exit_code(status: portable_pty::ExitStatus) -> i32 {
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

#[cfg(windows)]
#[inline(never)]
fn assign_child_to_windows_kill_on_close_job(
    handle: Option<std::os::windows::io::RawHandle>,
) -> PyResult<WindowsJobHandle> {
    running_process_core::rp_rust_debug_scope!(
        "running_process_py::assign_child_to_windows_kill_on_close_job"
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
        return Err(PyRuntimeError::new_err(
            "Pseudo-terminal child does not expose a Windows process handle",
        ));
    };

    let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
    if job.is_null() || job == INVALID_HANDLE_VALUE {
        return Err(to_py_err(std::io::Error::last_os_error()));
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
        return Err(to_py_err(err));
    }

    let result = unsafe { AssignProcessToJobObject(job, handle.cast()) };
    if result == FALSE {
        let err = std::io::Error::last_os_error();
        unsafe {
            winapi::um::handleapi::CloseHandle(job);
        }
        return Err(to_py_err(err));
    }

    Ok(WindowsJobHandle(job as usize))
}

#[cfg(windows)]
#[inline(never)]
fn apply_windows_pty_priority(
    handle: Option<std::os::windows::io::RawHandle>,
    nice: Option<i32>,
) -> PyResult<()> {
    running_process_core::rp_rust_debug_scope!("running_process_py::apply_windows_pty_priority");
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
        return Err(to_py_err(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[pymodule]
fn _native(_py: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyNativeProcess>()?;
    module.add_class::<NativeRunningProcess>()?;
    module.add_class::<NativePtyProcess>()?;
    module.add_class::<NativeProcessMetrics>()?;
    module.add_class::<NativeSignalBool>()?;
    module.add_class::<NativeIdleDetector>()?;
    module.add_class::<NativePtyBuffer>()?;
    module.add_function(wrap_pyfunction!(tracked_pid_db_path_py, module)?)?;
    module.add_function(wrap_pyfunction!(track_process_pid, module)?)?;
    module.add_function(wrap_pyfunction!(untrack_process_pid, module)?)?;
    module.add_function(wrap_pyfunction!(native_register_process, module)?)?;
    module.add_function(wrap_pyfunction!(native_unregister_process, module)?)?;
    module.add_function(wrap_pyfunction!(list_tracked_processes, module)?)?;
    module.add_function(wrap_pyfunction!(native_list_active_processes, module)?)?;
    module.add_function(wrap_pyfunction!(native_get_process_tree_info, module)?)?;
    module.add_function(wrap_pyfunction!(native_kill_process_tree, module)?)?;
    module.add_function(wrap_pyfunction!(native_process_created_at, module)?)?;
    module.add_function(wrap_pyfunction!(native_is_same_process, module)?)?;
    module.add_function(wrap_pyfunction!(native_cleanup_tracked_processes, module)?)?;
    module.add_function(wrap_pyfunction!(native_apply_process_nice, module)?)?;
    module.add_function(wrap_pyfunction!(
        native_windows_terminal_input_bytes,
        module
    )?)?;
    module.add_function(wrap_pyfunction!(native_dump_rust_debug_traces, module)?)?;
    module.add_function(wrap_pyfunction!(
        native_test_capture_rust_debug_trace,
        module
    )?)?;
    #[cfg(windows)]
    module.add_function(wrap_pyfunction!(native_test_hang_in_rust, module)?)?;
    module.add("VERSION", PyString::new(_py, env!("CARGO_PKG_VERSION")))?;
    Ok(())
}
