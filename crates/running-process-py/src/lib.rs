use std::collections::{HashMap, VecDeque};
#[cfg(windows)]
use std::fs;
use std::path::PathBuf;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::{Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use pyo3::exceptions::{PyRuntimeError, PyTimeoutError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyString};
use regex::Regex;
#[cfg(all(windows, test))]
use running_process_core::pty::terminal_input::{
    wait_for_terminal_input_event, TerminalInputWaitOutcome,
};
use running_process_core::pty::{
    self as core_pty,
    terminal_input::{
        self as core_terminal_input, TerminalInputCore, TerminalInputError,
        TerminalInputEventRecord,
    },
    IdleDetectorCore, IdleMonitorState, NativePtyProcess as CoreNativePtyProcess, PtyError,
};
use running_process_core::{
    find_processes_by_originator, render_rust_debug_traces, CommandSpec, ContainedChild,
    ContainedProcessGroup, Containment, NativeProcess, OriginatorProcessInfo, ProcessConfig,
    ProcessError, ReadStatus, StderrMode, StdinMode, StreamEvent, StreamKind,
};
#[cfg(unix)]
use running_process_core::{
    unix_set_priority, unix_signal_process, unix_signal_process_group, UnixSignal,
};
use sysinfo::{Pid, ProcessRefreshKind, Signal, System, UpdateKind};

mod daemon_client;
mod public_symbols;

// NATIVE_TERMINAL_INPUT_TRACE_PATH_ENV is now in running_process_core::pty::terminal_input
#[cfg(all(windows, test))]
use running_process_core::pty::terminal_input::NATIVE_TERMINAL_INPUT_TRACE_PATH_ENV;

fn to_py_err(err: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(err.to_string())
}

#[cfg(test)]
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
    // Build parent→children index in one pass.
    let mut children_map: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (child_pid, process) in system.processes() {
        if let Some(parent) = process.parent() {
            children_map.entry(parent).or_default().push(*child_pid);
        }
    }
    // BFS from pid.
    let mut descendants = Vec::new();
    let mut stack = vec![pid];
    while let Some(current) = stack.pop() {
        if let Some(children) = children_map.get(&current) {
            for &child in children {
                descendants.push(child);
                stack.push(child);
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

// unix_now_seconds is now in running_process_core::pty::terminal_input
fn unix_now_seconds() -> f64 {
    core_terminal_input::unix_now_seconds()
}

// native_terminal_input_trace_target, append_native_terminal_input_trace_line,
// and format_terminal_input_bytes are now in running_process_core::pty::terminal_input

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
            cwd: cwd.clone(),
            started_at,
        },
    );
    drop(registry); // release lock before IPC

    // Fire-and-forget daemon notification.
    daemon_client::daemon_register(pid, started_at, kind, command, cwd.as_deref());
}

fn unregister_active_process(pid: u32) {
    let mut registry = active_process_registry()
        .lock()
        .expect("active process registry mutex poisoned");
    registry.remove(&pid);
    drop(registry); // release lock before IPC

    // Fire-and-forget daemon notification.
    daemon_client::daemon_unregister(pid);
}

fn process_created_at(pid: u32) -> Option<f64> {
    let pid = system_pid(pid);
    let mut system = System::new();
    system.refresh_process_specifics(pid, ProcessRefreshKind::new());
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
    let snapshot: Vec<ActiveProcessRecord> = {
        let registry = active_process_registry()
            .lock()
            .expect("active process registry mutex poisoned");
        registry.values().cloned().collect()
    };
    let mut entries: Vec<_> = snapshot
        .into_iter()
        .map(|entry| {
            (
                entry.pid,
                process_created_at(entry.pid).unwrap_or(entry.started_at),
                entry.kind,
                entry.command,
                entry.cwd,
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
    let mut system = System::new();
    system.refresh_processes();
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

// windows_terminal_input_payload is now in running_process_core::pty
// native_terminal_input_mode, terminal_input_modifier_parameter,
// repeat_terminal_input_bytes, repeated_modified_sequence, repeated_tilde_sequence,
// control_character_for_unicode, trace_translated_console_key_event,
// translate_console_key_event, and native_terminal_input_worker
// are now in running_process_core::pty::terminal_input

// repeated_modified_sequence, repeated_tilde_sequence, control_character_for_unicode
// are now in running_process_core::pty::terminal_input

// trace_translated_console_key_event is now in running_process_core::pty::terminal_input

// translate_console_key_event and native_terminal_input_worker
// are now in running_process_core::pty::terminal_input

#[pyfunction]
fn native_get_process_tree_info(pid: u32) -> String {
    let mut system = System::new();
    system.refresh_processes();
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
        public_symbols::rp_windows_apply_process_priority_public(pid, nice)
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
fn windows_generate_console_ctrl_break_impl(pid: u32, creationflags: Option<u32>) -> PyResult<()> {
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
    let payload = core_pty::windows_terminal_input_payload(data);
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

// PTY types are now in running_process_core::pty

#[pyclass]
struct NativeProcessMetrics {
    pid: Pid,
    system: Mutex<System>,
}

#[pyclass]
struct NativePtyProcess {
    inner: CoreNativePtyProcess,
}

impl NativePtyProcess {
    fn pty_err_to_py(err: PtyError) -> PyErr {
        match err {
            PtyError::Timeout => PyTimeoutError::new_err(err.to_string()),
            _ => PyRuntimeError::new_err(err.to_string()),
        }
    }

    fn start_terminal_input_relay_py(&self) -> PyResult<()> {
        self.inner
            .start_terminal_input_relay_impl()
            .map_err(Self::pty_err_to_py)
    }
}

// WindowsJobHandle moved to running_process_core::pty

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

fn stderr_mode(name: &str) -> PyResult<StderrMode> {
    match name {
        "stdout" => Ok(StderrMode::Stdout),
        "pipe" => Ok(StderrMode::Pipe),
        _ => Err(PyValueError::new_err(
            "stderr_mode must be 'stdout' or 'pipe'",
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

// IdleMonitorState and IdleDetectorCore are now in running_process_core::pty

// IdleDetectorCore impl is now in running_process_core::pty

#[pyclass]
struct NativeIdleDetector {
    core: Arc<IdleDetectorCore>,
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

// TerminalInputEventRecord, TerminalInputState, ActiveTerminalInputCapture,
// TerminalInputWaitOutcome, and wait_for_terminal_input_event
// are now in running_process_core::pty::terminal_input

// PTY helper functions (input_contains_newline, record_pty_input_metrics,
// store_pty_returncode, poll_pty_process, write_pty_input) are now in
// running_process_core::pty

#[pyclass]
#[derive(Clone)]
struct NativeTerminalInputEvent {
    data: Vec<u8>,
    submit: bool,
    shift: bool,
    ctrl: bool,
    alt: bool,
    virtual_key_code: u16,
    repeat_count: u16,
}

#[pyclass]
struct NativeTerminalInput {
    inner: TerminalInputCore,
}

#[pymethods]
impl NativeRunningProcess {
    #[new]
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (command, cwd=None, shell=false, capture=true, env=None, creationflags=None, text=true, encoding=None, errors=None, stdin_mode_name="inherit", stderr_mode_name="stdout", nice=None, create_process_group=false))]
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
        stderr_mode_name: &str,
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
                stderr_mode: stderr_mode(stderr_mode_name)?,
                creationflags,
                create_process_group,
                stdin_mode: stdin_mode(stdin_mode_name)?,
                nice,
                containment: None,
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
        let compiled_regex = if is_regex {
            Some(Regex::new(pattern).map_err(to_py_err)?)
        } else {
            None
        };

        loop {
            if let Some((matched, start, end, groups)) =
                self.find_expect_match(&buffer, pattern, compiled_regex.as_ref())?
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
    #[pyo3(signature = (command, cwd=None, shell=false, capture=true, env=None, creationflags=None, text=true, encoding=None, errors=None, stdin_mode_name="inherit", stderr_mode_name="stdout", nice=None, create_process_group=false))]
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
        stderr_mode_name: &str,
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
                stderr_mode_name,
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
        let env_pairs = env
            .map(|mapping| {
                mapping
                    .iter()
                    .map(|(key, value)| Ok((key.extract::<String>()?, value.extract::<String>()?)))
                    .collect::<PyResult<Vec<(String, String)>>>()
            })
            .transpose()?;
        let inner = CoreNativePtyProcess::new(argv, cwd, env_pairs, rows, cols, nice)
            .map_err(NativePtyProcess::pty_err_to_py)?;
        Ok(Self {
            backend: NativeProcessBackend::Pty(NativePtyProcess { inner }),
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

    fn kill(&self, py: Python<'_>) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.kill(),
            NativeProcessBackend::Pty(process) => process.kill(py),
        }
    }

    fn terminate(&self, py: Python<'_>) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.terminate(),
            NativeProcessBackend::Pty(process) => process.terminate(py),
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
            NativeProcessBackend::Pty(process) => process.write(data, false),
        }
    }

    #[pyo3(signature = (data, submit=false))]
    fn write(&self, data: &[u8], submit: bool) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.write_stdin(data),
            NativeProcessBackend::Pty(process) => process.write(data, submit),
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

    fn start_terminal_input_relay(&self) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => process.start_terminal_input_relay(),
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "terminal input relay is only available for PTY-backed NativeProcess",
            )),
        }
    }

    fn stop_terminal_input_relay(&self) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => {
                process.stop_terminal_input_relay();
                Ok(())
            }
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "terminal input relay is only available for PTY-backed NativeProcess",
            )),
        }
    }

    fn terminal_input_relay_active(&self) -> PyResult<bool> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => Ok(process.terminal_input_relay_active()),
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "terminal input relay is only available for PTY-backed NativeProcess",
            )),
        }
    }

    fn pty_input_bytes_total(&self) -> PyResult<usize> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => Ok(process.pty_input_bytes_total()),
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "PTY input metrics are only available for PTY-backed NativeProcess",
            )),
        }
    }

    fn pty_newline_events_total(&self) -> PyResult<usize> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => Ok(process.pty_newline_events_total()),
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "PTY input metrics are only available for PTY-backed NativeProcess",
            )),
        }
    }

    fn pty_submit_events_total(&self) -> PyResult<usize> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => Ok(process.pty_submit_events_total()),
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "PTY input metrics are only available for PTY-backed NativeProcess",
            )),
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
                .inner
                .returncode
                .lock()
                .expect("pty returncode mutex poisoned")),
        }
    }

    fn is_pty(&self) -> bool {
        matches!(self.backend, NativeProcessBackend::Pty(_))
    }

    /// Wait for exit then drain remaining output (PTY only).
    #[pyo3(signature = (timeout=None, drain_timeout=2.0))]
    fn wait_and_drain(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
        drain_timeout: f64,
    ) -> PyResult<i32> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => {
                process.wait_and_drain(py, timeout, drain_timeout)
            }
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "wait_and_drain is only available for PTY-backed NativeProcess",
            )),
        }
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
        let env_pairs = env
            .map(|mapping| {
                mapping
                    .iter()
                    .map(|(key, value)| Ok((key.extract::<String>()?, value.extract::<String>()?)))
                    .collect::<PyResult<Vec<(String, String)>>>()
            })
            .transpose()?;
        let inner = CoreNativePtyProcess::new(argv, cwd, env_pairs, rows, cols, nice)
            .map_err(Self::pty_err_to_py)?;
        Ok(Self { inner })
    }

    #[inline(never)]
    fn start(&self) -> PyResult<()> {
        self.inner.start_impl().map_err(Self::pty_err_to_py)
    }

    #[pyo3(signature = (timeout=None))]
    fn read_chunk(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<Py<PyAny>> {
        let result = py.allow_threads(|| self.inner.read_chunk_impl(timeout));
        match result {
            Ok(Some(chunk)) => Ok(PyBytes::new(py, &chunk).into_any().unbind()),
            Ok(None) => Err(PyTimeoutError::new_err(
                "No pseudo-terminal output available before timeout",
            )),
            Err(e) => Err(Self::pty_err_to_py(e)),
        }
    }

    #[pyo3(signature = (timeout=None))]
    fn wait_for_reader_closed(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<bool> {
        Ok(py.allow_threads(|| self.inner.wait_for_reader_closed_impl(timeout)))
    }

    #[pyo3(signature = (data, submit=false))]
    fn write(&self, data: &[u8], submit: bool) -> PyResult<()> {
        self.inner
            .write_impl(data, submit)
            .map_err(Self::pty_err_to_py)
    }

    fn respond_to_queries(&self, data: &[u8]) -> PyResult<()> {
        self.inner
            .respond_to_queries_impl(data)
            .map_err(Self::pty_err_to_py)
    }

    #[inline(never)]
    fn resize(&self, rows: u16, cols: u16) -> PyResult<()> {
        self.inner
            .resize_impl(rows, cols)
            .map_err(Self::pty_err_to_py)
    }

    #[inline(never)]
    fn send_interrupt(&self) -> PyResult<()> {
        self.inner
            .send_interrupt_impl()
            .map_err(Self::pty_err_to_py)
    }

    fn poll(&self) -> PyResult<Option<i32>> {
        core_pty::poll_pty_process(&self.inner.handles, &self.inner.returncode).map_err(to_py_err)
    }

    #[pyo3(signature = (timeout=None))]
    #[inline(never)]
    fn wait(&self, timeout: Option<f64>) -> PyResult<i32> {
        self.inner.wait_impl(timeout).map_err(Self::pty_err_to_py)
    }

    #[inline(never)]
    fn terminate(&self, py: Python<'_>) -> PyResult<()> {
        py.allow_threads(|| self.inner.terminate_impl().map_err(Self::pty_err_to_py))
    }

    #[inline(never)]
    fn kill(&self, py: Python<'_>) -> PyResult<()> {
        py.allow_threads(|| self.inner.kill_impl().map_err(Self::pty_err_to_py))
    }

    #[inline(never)]
    fn terminate_tree(&self) -> PyResult<()> {
        self.inner
            .terminate_tree_impl()
            .map_err(Self::pty_err_to_py)
    }

    #[inline(never)]
    fn kill_tree(&self) -> PyResult<()> {
        self.inner.kill_tree_impl().map_err(Self::pty_err_to_py)
    }

    fn start_terminal_input_relay(&self) -> PyResult<()> {
        self.start_terminal_input_relay_py()
    }

    fn stop_terminal_input_relay(&self) {
        self.inner.stop_terminal_input_relay_impl();
    }

    fn terminal_input_relay_active(&self) -> bool {
        self.inner.terminal_input_relay_active()
    }

    fn pty_input_bytes_total(&self) -> usize {
        self.inner.pty_input_bytes_total()
    }

    fn pty_newline_events_total(&self) -> usize {
        self.inner.pty_newline_events_total()
    }

    fn pty_submit_events_total(&self) -> usize {
        self.inner.pty_submit_events_total()
    }

    fn pty_output_bytes_total(&self) -> usize {
        self.inner.pty_output_bytes_total()
    }

    fn pty_control_churn_bytes_total(&self) -> usize {
        self.inner.pty_control_churn_bytes_total()
    }

    #[pyo3(signature = (timeout=None, drain_timeout=2.0))]
    fn wait_and_drain(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
        drain_timeout: f64,
    ) -> PyResult<i32> {
        py.allow_threads(|| {
            self.inner
                .wait_and_drain_impl(timeout, drain_timeout)
                .map_err(Self::pty_err_to_py)
        })
    }

    fn set_echo(&self, enabled: bool) {
        self.inner.set_echo(enabled);
    }

    fn echo_enabled(&self) -> bool {
        self.inner.echo_enabled()
    }

    fn attach_idle_detector(&self, detector: &NativeIdleDetector) {
        self.inner.attach_idle_detector(&detector.core);
    }

    fn detach_idle_detector(&self) {
        self.inner.detach_idle_detector();
    }

    #[pyo3(signature = (detector, timeout=None))]
    fn wait_for_idle(
        &self,
        py: Python<'_>,
        detector: &NativeIdleDetector,
        timeout: Option<f64>,
    ) -> PyResult<(bool, String, f64, Option<i32>)> {
        // Wire the detector into the reader thread.
        self.inner.attach_idle_detector(&detector.core);

        // Spawn exit watcher that marks the detector on process exit.
        let handles = Arc::clone(&self.inner.handles);
        let returncode = Arc::clone(&self.inner.returncode);
        let core = Arc::clone(&detector.core);
        let exit_watcher = thread::spawn(move || loop {
            match core_pty::poll_pty_process(&handles, &returncode) {
                Ok(Some(code)) => {
                    let interrupted = code == -2 || code == 130;
                    core.mark_exit(code, interrupted);
                    return;
                }
                Ok(None) => {}
                Err(_) => return,
            }
            thread::sleep(Duration::from_millis(1));
        });

        let result = py.allow_threads(|| detector.core.wait(timeout));

        self.inner.detach_idle_detector();
        let _ = exit_watcher.join();
        Ok(result)
    }

    #[getter]
    fn pid(&self) -> PyResult<Option<u32>> {
        self.inner.pid().map_err(Self::pty_err_to_py)
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        py.allow_threads(|| self.inner.close_impl().map_err(Self::pty_err_to_py))
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

impl NativeTerminalInput {
    fn event_to_py(
        py: Python<'_>,
        event: TerminalInputEventRecord,
    ) -> PyResult<Py<NativeTerminalInputEvent>> {
        Py::new(
            py,
            NativeTerminalInputEvent {
                data: event.data,
                submit: event.submit,
                shift: event.shift,
                ctrl: event.ctrl,
                alt: event.alt,
                virtual_key_code: event.virtual_key_code,
                repeat_count: event.repeat_count,
            },
        )
    }

    fn wait_for_event(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> PyResult<TerminalInputEventRecord> {
        py.allow_threads(|| {
            self.inner.wait_for_event(timeout).map_err(|err| match err {
                TerminalInputError::Closed => {
                    PyRuntimeError::new_err("Native terminal input is closed")
                }
                TerminalInputError::Timeout => {
                    PyTimeoutError::new_err("No terminal input available before timeout")
                }
                other => to_py_err(other),
            })
        })
    }
}

#[pymethods]
impl NativeTerminalInputEvent {
    #[getter]
    fn data(&self, py: Python<'_>) -> Py<PyAny> {
        PyBytes::new(py, &self.data).into_any().unbind()
    }

    #[getter]
    fn submit(&self) -> bool {
        self.submit
    }

    #[getter]
    fn shift(&self) -> bool {
        self.shift
    }

    #[getter]
    fn ctrl(&self) -> bool {
        self.ctrl
    }

    #[getter]
    fn alt(&self) -> bool {
        self.alt
    }

    #[getter]
    fn virtual_key_code(&self) -> u16 {
        self.virtual_key_code
    }

    #[getter]
    fn repeat_count(&self) -> u16 {
        self.repeat_count
    }

    fn __repr__(&self) -> String {
        format!(
            "NativeTerminalInputEvent(data={:?}, submit={}, shift={}, ctrl={}, alt={}, virtual_key_code={}, repeat_count={})",
            self.data,
            self.submit,
            self.shift,
            self.ctrl,
            self.alt,
            self.virtual_key_code,
            self.repeat_count,
        )
    }
}

#[pymethods]
impl NativeTerminalInput {
    #[new]
    fn new() -> Self {
        Self {
            inner: TerminalInputCore::new(),
        }
    }

    fn start(&self) -> PyResult<()> {
        #[cfg(windows)]
        {
            self.inner.start_impl().map_err(to_py_err)
        }

        #[cfg(not(windows))]
        {
            Err(PyRuntimeError::new_err(
                "NativeTerminalInput is only available on Windows consoles",
            ))
        }
    }

    fn stop(&self, py: Python<'_>) -> PyResult<()> {
        py.allow_threads(|| self.inner.stop_impl().map_err(to_py_err))
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        py.allow_threads(|| self.inner.stop_impl().map_err(to_py_err))
    }

    fn available(&self) -> bool {
        self.inner.available()
    }

    #[getter]
    fn capturing(&self) -> bool {
        self.inner.capturing()
    }

    #[getter]
    fn original_console_mode(&self) -> Option<u32> {
        self.inner.original_console_mode()
    }

    #[getter]
    fn active_console_mode(&self) -> Option<u32> {
        self.inner.active_console_mode()
    }

    #[pyo3(signature = (timeout=None))]
    fn read_event(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> PyResult<Py<NativeTerminalInputEvent>> {
        let event = self.wait_for_event(py, timeout)?;
        Self::event_to_py(py, event)
    }

    fn read_event_non_blocking(
        &self,
        py: Python<'_>,
    ) -> PyResult<Option<Py<NativeTerminalInputEvent>>> {
        if let Some(event) = self.inner.next_event() {
            return Self::event_to_py(py, event).map(Some);
        }
        if self
            .inner
            .state
            .lock()
            .expect("terminal input mutex poisoned")
            .closed
        {
            return Err(PyRuntimeError::new_err("Native terminal input is closed"));
        }
        Ok(None)
    }

    #[pyo3(signature = (timeout=None))]
    fn read(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<Py<PyAny>> {
        let event = self.wait_for_event(py, timeout)?;
        Ok(PyBytes::new(py, &event.data).into_any().unbind())
    }

    fn read_non_blocking(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        if let Some(event) = self.inner.next_event() {
            return Ok(Some(PyBytes::new(py, &event.data).into_any().unbind()));
        }
        if self
            .inner
            .state
            .lock()
            .expect("terminal input mutex poisoned")
            .closed
        {
            return Err(PyRuntimeError::new_err("Native terminal input is closed"));
        }
        Ok(None)
    }

    fn drain(&self, py: Python<'_>) -> Vec<Py<PyAny>> {
        self.inner
            .drain_events()
            .into_iter()
            .map(|event| PyBytes::new(py, &event.data).into_any().unbind())
            .collect()
    }

    fn drain_events(&self, py: Python<'_>) -> PyResult<Vec<Py<NativeTerminalInputEvent>>> {
        self.inner
            .drain_events()
            .into_iter()
            .map(|event| Self::event_to_py(py, event))
            .collect()
    }

    /// Wait for at least one input event, then drain all queued events and
    /// return their data merged into a single `bytes` object plus a `submit`
    /// flag.  This avoids per-event Python round-trips during large pastes.
    ///
    /// Returns ``(data: bytes, submit: bool)``.
    #[pyo3(signature = (timeout=None))]
    fn read_batch(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<(Py<PyAny>, bool)> {
        // Block (releasing the GIL) until the first event arrives.
        let first = self.wait_for_event(py, timeout)?;

        // Drain everything else already queued.
        let remaining = self.inner.drain_events();

        // Merge all data into one buffer.
        let capacity = first.data.len() + remaining.iter().map(|e| e.data.len()).sum::<usize>();
        let mut merged = Vec::with_capacity(capacity);
        let mut submit = first.submit;
        merged.extend_from_slice(&first.data);
        for event in &remaining {
            merged.extend_from_slice(&event.data);
            submit = submit || event.submit;
        }

        Ok((PyBytes::new(py, &merged).into_any().unbind(), submit))
    }
}

// Drop is now handled by TerminalInputCore's Drop impl

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
            public_symbols::rp_windows_generate_console_ctrl_break_public(pid, self.creationflags)
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
        let encoding = self.encoding.as_deref().unwrap_or("utf-8");
        let errors = self.errors.as_deref().unwrap_or("replace");
        if encoding == "utf-8" && errors == "replace" {
            return Ok(String::from_utf8_lossy(line).into_owned());
        }
        PyBytes::new(py, line)
            .call_method1("decode", (encoding, errors))?
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
        compiled_regex: Option<&Regex>,
    ) -> PyResult<Option<ExpectDetails>> {
        if compiled_regex.is_none() {
            // Literal string match
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

        let regex = compiled_regex.unwrap();
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
        let encoding = self.encoding.as_deref().unwrap_or("utf-8");
        let errors = self.errors.as_deref().unwrap_or("replace");
        if encoding == "utf-8" && errors == "replace" {
            let s = String::from_utf8_lossy(line);
            return Ok(PyString::new(py, &s).into_any().unbind());
        }
        Ok(PyBytes::new(py, line)
            .call_method1("decode", (encoding, errors))?
            .into_any()
            .unbind())
    }
}

impl NativePtyBuffer {
    fn decode_chunk(&self, py: Python<'_>, line: &[u8]) -> PyResult<Py<PyAny>> {
        if !self.text {
            return Ok(PyBytes::new(py, line).into_any().unbind());
        }
        if self.encoding == "utf-8" && self.errors == "replace" {
            let s = String::from_utf8_lossy(line);
            return Ok(PyString::new(py, &s).into_any().unbind());
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
            core: Arc::new(IdleDetectorCore {
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
            }),
        }
    }

    #[getter]
    fn enabled(&self) -> bool {
        self.core.enabled()
    }

    #[setter]
    fn set_enabled(&self, enabled: bool) {
        self.core.set_enabled(enabled);
    }

    fn record_input(&self, byte_count: usize) {
        self.core.record_input(byte_count);
    }

    fn record_output(&self, data: &[u8]) {
        self.core.record_output(data);
    }

    fn mark_exit(&self, returncode: i32, interrupted: bool) {
        self.core.mark_exit(returncode, interrupted);
    }

    #[pyo3(signature = (timeout=None))]
    fn wait(&self, py: Python<'_>, timeout: Option<f64>) -> (bool, String, f64, Option<i32>) {
        py.allow_threads(|| self.core.wait(timeout))
    }
}

// PTY helper functions (control_churn_bytes, command_builder_from_argv,
// spawn_pty_reader, portable_exit_code, assign_child_to_windows_kill_on_close_job,
// apply_windows_pty_priority) are now in running_process_core::pty

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use running_process_core::pty::terminal_input::{
        control_character_for_unicode, format_terminal_input_bytes, native_terminal_input_mode,
        native_terminal_input_trace_target, repeat_terminal_input_bytes,
        repeated_modified_sequence, repeated_tilde_sequence, terminal_input_modifier_parameter,
        translate_console_key_event, TerminalInputState,
    };
    use running_process_core::pty::{
        self as core_pty, NativePtyHandles, NativePtyProcess as CoreNativePtyProcess,
        PtyReadShared, PtyReadState,
    };
    #[cfg(windows)]
    use running_process_core::pty::{
        apply_windows_pty_priority, assign_child_to_windows_kill_on_close_job,
    };

    #[cfg(windows)]
    use winapi::um::wincon::{
        ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_QUICK_EDIT_MODE, ENABLE_WINDOW_INPUT,
    };
    #[cfg(windows)]
    use winapi::um::wincontypes::{
        KEY_EVENT_RECORD, LEFT_ALT_PRESSED, LEFT_CTRL_PRESSED, SHIFT_PRESSED,
    };
    #[cfg(windows)]
    use winapi::um::winuser::{VK_RETURN, VK_TAB, VK_UP};

    #[cfg(windows)]
    fn key_event(
        virtual_key_code: u16,
        unicode: u16,
        control_key_state: u32,
        repeat_count: u16,
    ) -> KEY_EVENT_RECORD {
        let mut event: KEY_EVENT_RECORD = unsafe { std::mem::zeroed() };
        event.bKeyDown = 1;
        event.wRepeatCount = repeat_count;
        event.wVirtualKeyCode = virtual_key_code;
        event.wVirtualScanCode = 0;
        event.dwControlKeyState = control_key_state;
        unsafe {
            *event.uChar.UnicodeChar_mut() = unicode;
        }
        event
    }

    #[test]
    #[cfg(windows)]
    fn native_terminal_input_mode_disables_cooked_console_flags() {
        let original_mode =
            ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT | ENABLE_QUICK_EDIT_MODE;

        let active_mode = native_terminal_input_mode(original_mode);

        assert_eq!(active_mode & ENABLE_ECHO_INPUT, 0);
        assert_eq!(active_mode & ENABLE_LINE_INPUT, 0);
        assert_eq!(active_mode & ENABLE_PROCESSED_INPUT, 0);
        assert_eq!(active_mode & ENABLE_QUICK_EDIT_MODE, 0);
        assert_ne!(active_mode & ENABLE_EXTENDED_FLAGS, 0);
        assert_ne!(active_mode & ENABLE_WINDOW_INPUT, 0);
    }

    #[test]
    #[cfg(windows)]
    fn translate_terminal_input_preserves_submit_hint_for_enter() {
        let event = translate_console_key_event(&key_event(VK_RETURN as u16, '\r' as u16, 0, 1))
            .expect("enter should translate");
        assert_eq!(event.data, b"\r");
        assert!(event.submit);
    }

    #[test]
    #[cfg(windows)]
    fn translate_terminal_input_keeps_shift_enter_non_submit() {
        let event = translate_console_key_event(&key_event(
            VK_RETURN as u16,
            '\r' as u16,
            SHIFT_PRESSED,
            1,
        ))
        .expect("shift-enter should translate");
        // Shift+Enter emits CSI u sequence so downstream apps can
        // distinguish it from plain Enter.
        assert_eq!(event.data, b"\x1b[13;2u");
        assert!(!event.submit);
        assert!(event.shift);
    }

    #[test]
    #[cfg(windows)]
    fn translate_terminal_input_encodes_shift_tab() {
        let event = translate_console_key_event(&key_event(VK_TAB as u16, 0, SHIFT_PRESSED, 1))
            .expect("shift-tab should translate");
        assert_eq!(event.data, b"\x1b[Z");
        assert!(!event.submit);
    }

    #[test]
    #[cfg(windows)]
    fn translate_terminal_input_encodes_modified_arrows() {
        let event = translate_console_key_event(&key_event(
            VK_UP as u16,
            0,
            SHIFT_PRESSED | LEFT_CTRL_PRESSED,
            1,
        ))
        .expect("modified arrow should translate");
        assert_eq!(event.data, b"\x1b[1;6A");
    }

    #[test]
    #[cfg(windows)]
    fn translate_terminal_input_encodes_alt_printable_with_escape_prefix() {
        let event =
            translate_console_key_event(&key_event(b'X' as u16, 'x' as u16, LEFT_ALT_PRESSED, 1))
                .expect("alt printable should translate");
        assert_eq!(event.data, b"\x1bx");
    }

    #[test]
    #[cfg(windows)]
    fn translate_terminal_input_encodes_ctrl_printable_as_control_character() {
        let event =
            translate_console_key_event(&key_event(b'C' as u16, 'c' as u16, LEFT_CTRL_PRESSED, 1))
                .expect("ctrl-c should translate");
        assert_eq!(event.data, [0x03]);
    }

    #[test]
    #[cfg(windows)]
    fn translate_terminal_input_ignores_keyup_events() {
        let mut event = key_event(VK_RETURN as u16, '\r' as u16, 0, 1);
        event.bKeyDown = 0;
        assert!(translate_console_key_event(&event).is_none());
    }

    // ── control_churn_bytes tests ──

    #[test]
    fn control_churn_bytes_empty() {
        assert_eq!(core_pty::control_churn_bytes(b""), 0);
    }

    #[test]
    fn control_churn_bytes_plain_text() {
        assert_eq!(core_pty::control_churn_bytes(b"hello world"), 0);
    }

    #[test]
    fn control_churn_bytes_ansi_csi_sequence() {
        // \x1b[31m = 5 bytes of control churn, \x1b[0m = 4 bytes
        assert_eq!(core_pty::control_churn_bytes(b"\x1b[31mhello\x1b[0m"), 9);
    }

    #[test]
    fn control_churn_bytes_backspace_cr_del() {
        assert_eq!(core_pty::control_churn_bytes(b"\x08\x0D\x7F"), 3);
    }

    #[test]
    fn control_churn_bytes_bare_escape() {
        // Bare ESC with no CSI sequence following
        assert_eq!(core_pty::control_churn_bytes(b"\x1b"), 1);
    }

    #[test]
    fn control_churn_bytes_mixed() {
        // \x1b[J = 3 bytes CSI + 1 byte BS = 4
        assert_eq!(core_pty::control_churn_bytes(b"ok\x1b[Jmore\x08"), 4);
    }

    // ── input_contains_newline tests ──

    #[test]
    fn input_contains_newline_cr() {
        assert!(core_pty::input_contains_newline(b"hello\rworld"));
    }

    #[test]
    fn input_contains_newline_lf() {
        assert!(core_pty::input_contains_newline(b"hello\nworld"));
    }

    #[test]
    fn input_contains_newline_none() {
        assert!(!core_pty::input_contains_newline(b"hello world"));
    }

    #[test]
    fn input_contains_newline_empty() {
        assert!(!core_pty::input_contains_newline(b""));
    }

    // ── is_ignorable_process_control_error tests ──

    #[test]
    fn ignorable_error_not_found() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        assert!(is_ignorable_process_control_error(&err));
    }

    #[test]
    fn ignorable_error_invalid_input() {
        let err = std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad input");
        assert!(is_ignorable_process_control_error(&err));
    }

    #[test]
    fn ignorable_error_permission_denied_is_not_ignorable() {
        let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        assert!(!is_ignorable_process_control_error(&err));
    }

    #[test]
    #[cfg(unix)]
    fn ignorable_error_esrch() {
        let err = std::io::Error::from_raw_os_error(libc::ESRCH);
        assert!(is_ignorable_process_control_error(&err));
    }

    // ── Windows-only pure function tests ──

    #[test]
    #[cfg(windows)]
    fn windows_terminal_input_payload_passthrough() {
        let result = core_pty::windows_terminal_input_payload(b"hello");
        assert_eq!(result, b"hello");
    }

    #[test]
    #[cfg(windows)]
    fn windows_terminal_input_payload_lone_lf_becomes_cr() {
        let result = core_pty::windows_terminal_input_payload(b"\n");
        assert_eq!(result, b"\r");
    }

    #[test]
    #[cfg(windows)]
    fn windows_terminal_input_payload_crlf_preserved() {
        let result = core_pty::windows_terminal_input_payload(b"\r\n");
        assert_eq!(result, b"\r\n");
    }

    #[test]
    #[cfg(windows)]
    fn windows_terminal_input_payload_lone_cr_preserved() {
        let result = core_pty::windows_terminal_input_payload(b"\r");
        assert_eq!(result, b"\r");
    }

    #[test]
    #[cfg(windows)]
    fn terminal_input_modifier_none() {
        assert!(terminal_input_modifier_parameter(false, false, false).is_none());
    }

    #[test]
    #[cfg(windows)]
    fn terminal_input_modifier_shift() {
        assert_eq!(
            terminal_input_modifier_parameter(true, false, false),
            Some(2)
        );
    }

    #[test]
    #[cfg(windows)]
    fn terminal_input_modifier_alt() {
        assert_eq!(
            terminal_input_modifier_parameter(false, true, false),
            Some(3)
        );
    }

    #[test]
    #[cfg(windows)]
    fn terminal_input_modifier_ctrl() {
        assert_eq!(
            terminal_input_modifier_parameter(false, false, true),
            Some(5)
        );
    }

    #[test]
    #[cfg(windows)]
    fn terminal_input_modifier_shift_ctrl() {
        assert_eq!(
            terminal_input_modifier_parameter(true, false, true),
            Some(6)
        );
    }

    #[test]
    #[cfg(windows)]
    fn control_character_for_unicode_letters() {
        assert_eq!(control_character_for_unicode('A' as u16), Some(0x01));
        assert_eq!(control_character_for_unicode('C' as u16), Some(0x03));
        assert_eq!(control_character_for_unicode('Z' as u16), Some(0x1A));
    }

    #[test]
    #[cfg(windows)]
    fn control_character_for_unicode_special() {
        assert_eq!(control_character_for_unicode('@' as u16), Some(0x00));
        assert_eq!(control_character_for_unicode('[' as u16), Some(0x1B));
    }

    #[test]
    #[cfg(windows)]
    fn control_character_for_unicode_digit_returns_none() {
        assert!(control_character_for_unicode('1' as u16).is_none());
    }

    #[test]
    #[cfg(windows)]
    fn format_terminal_input_bytes_empty() {
        assert_eq!(format_terminal_input_bytes(b""), "[]");
    }

    #[test]
    #[cfg(windows)]
    fn format_terminal_input_bytes_multi() {
        assert_eq!(format_terminal_input_bytes(&[0x41, 0x42]), "[41 42]");
    }

    #[test]
    #[cfg(windows)]
    fn repeated_tilde_sequence_no_modifier() {
        assert_eq!(repeated_tilde_sequence(3, None, 1), b"\x1b[3~");
    }

    #[test]
    #[cfg(windows)]
    fn repeated_tilde_sequence_with_modifier() {
        assert_eq!(repeated_tilde_sequence(3, Some(2), 1), b"\x1b[3;2~");
    }

    #[test]
    #[cfg(windows)]
    fn repeated_tilde_sequence_repeated() {
        let result = repeated_tilde_sequence(3, None, 3);
        assert_eq!(result, b"\x1b[3~\x1b[3~\x1b[3~");
    }

    #[test]
    #[cfg(windows)]
    fn repeated_modified_sequence_no_modifier() {
        let result = repeated_modified_sequence(b"\x1b[A", None, 1);
        assert_eq!(result, b"\x1b[A");
    }

    #[test]
    #[cfg(windows)]
    fn repeated_modified_sequence_with_modifier() {
        // Shift modifier (2) applied to Up arrow
        let result = repeated_modified_sequence(b"\x1b[A", Some(2), 1);
        assert_eq!(result, b"\x1b[1;2A");
    }

    #[test]
    #[cfg(windows)]
    fn repeated_modified_sequence_repeated() {
        let result = repeated_modified_sequence(b"\x1b[A", None, 2);
        assert_eq!(result, b"\x1b[A\x1b[A");
    }

    #[test]
    #[cfg(windows)]
    fn repeat_terminal_input_bytes_single() {
        let result = repeat_terminal_input_bytes(b"\r", 1);
        assert_eq!(result, b"\r");
    }

    #[test]
    #[cfg(windows)]
    fn repeat_terminal_input_bytes_multiple() {
        let result = repeat_terminal_input_bytes(b"ab", 3);
        assert_eq!(result, b"ababab");
    }

    #[test]
    #[cfg(windows)]
    fn repeat_terminal_input_bytes_zero_clamps_to_one() {
        let result = repeat_terminal_input_bytes(b"x", 0);
        assert_eq!(result, b"x");
    }

    // ── B1: Windows Console Key Translation (navigation keys) ──

    #[test]
    #[cfg(windows)]
    fn translate_console_key_home() {
        use winapi::um::winuser::VK_HOME;
        let event = translate_console_key_event(&key_event(VK_HOME as u16, 0, 0, 1))
            .expect("VK_HOME should translate");
        assert_eq!(event.data, b"\x1b[H");
        assert!(!event.submit);
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_end() {
        use winapi::um::winuser::VK_END;
        let event = translate_console_key_event(&key_event(VK_END as u16, 0, 0, 1))
            .expect("VK_END should translate");
        assert_eq!(event.data, b"\x1b[F");
        assert!(!event.submit);
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_insert() {
        use winapi::um::winuser::VK_INSERT;
        let event = translate_console_key_event(&key_event(VK_INSERT as u16, 0, 0, 1))
            .expect("VK_INSERT should translate");
        assert_eq!(event.data, b"\x1b[2~");
        assert!(!event.submit);
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_delete() {
        use winapi::um::winuser::VK_DELETE;
        let event = translate_console_key_event(&key_event(VK_DELETE as u16, 0, 0, 1))
            .expect("VK_DELETE should translate");
        assert_eq!(event.data, b"\x1b[3~");
        assert!(!event.submit);
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_page_up() {
        use winapi::um::winuser::VK_PRIOR;
        let event = translate_console_key_event(&key_event(VK_PRIOR as u16, 0, 0, 1))
            .expect("VK_PRIOR should translate");
        assert_eq!(event.data, b"\x1b[5~");
        assert!(!event.submit);
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_page_down() {
        use winapi::um::winuser::VK_NEXT;
        let event = translate_console_key_event(&key_event(VK_NEXT as u16, 0, 0, 1))
            .expect("VK_NEXT should translate");
        assert_eq!(event.data, b"\x1b[6~");
        assert!(!event.submit);
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_shift_home() {
        use winapi::um::winuser::VK_HOME;
        let event = translate_console_key_event(&key_event(VK_HOME as u16, 0, SHIFT_PRESSED, 1))
            .expect("Shift+Home should translate");
        assert_eq!(event.data, b"\x1b[1;2H");
        assert!(event.shift);
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_shift_end() {
        use winapi::um::winuser::VK_END;
        let event = translate_console_key_event(&key_event(VK_END as u16, 0, SHIFT_PRESSED, 1))
            .expect("Shift+End should translate");
        assert_eq!(event.data, b"\x1b[1;2F");
        assert!(event.shift);
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_ctrl_home() {
        use winapi::um::winuser::VK_HOME;
        let event =
            translate_console_key_event(&key_event(VK_HOME as u16, 0, LEFT_CTRL_PRESSED, 1))
                .expect("Ctrl+Home should translate");
        assert_eq!(event.data, b"\x1b[1;5H");
        assert!(event.ctrl);
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_shift_delete() {
        use winapi::um::winuser::VK_DELETE;
        let event = translate_console_key_event(&key_event(VK_DELETE as u16, 0, SHIFT_PRESSED, 1))
            .expect("Shift+Delete should translate");
        assert_eq!(event.data, b"\x1b[3;2~");
        assert!(event.shift);
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_ctrl_page_up() {
        use winapi::um::winuser::VK_PRIOR;
        let event =
            translate_console_key_event(&key_event(VK_PRIOR as u16, 0, LEFT_CTRL_PRESSED, 1))
                .expect("Ctrl+PageUp should translate");
        assert_eq!(event.data, b"\x1b[5;5~");
        assert!(event.ctrl);
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_backspace() {
        use winapi::um::winuser::VK_BACK;
        let event = translate_console_key_event(&key_event(VK_BACK as u16, 0x08, 0, 1))
            .expect("Backspace should translate");
        assert_eq!(event.data, b"\x08");
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_escape() {
        use winapi::um::winuser::VK_ESCAPE;
        let event = translate_console_key_event(&key_event(VK_ESCAPE as u16, 0x1b, 0, 1))
            .expect("Escape should translate");
        assert_eq!(event.data, b"\x1b");
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_tab() {
        let event = translate_console_key_event(&key_event(VK_TAB as u16, 0, 0, 1))
            .expect("Tab should translate");
        assert_eq!(event.data, b"\t");
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_plain_enter_is_submit() {
        let event = translate_console_key_event(&key_event(VK_RETURN as u16, '\r' as u16, 0, 1))
            .expect("Enter should translate");
        assert_eq!(event.data, b"\r");
        assert!(event.submit);
        assert!(!event.shift);
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_unicode_printable() {
        // Regular 'a' key
        let event = translate_console_key_event(&key_event(b'A' as u16, 'a' as u16, 0, 1))
            .expect("printable should translate");
        assert_eq!(event.data, b"a");
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_unicode_repeated() {
        let event = translate_console_key_event(&key_event(b'A' as u16, 'a' as u16, 0, 3))
            .expect("repeated printable should translate");
        assert_eq!(event.data, b"aaa");
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_down_arrow() {
        use winapi::um::winuser::VK_DOWN;
        let event = translate_console_key_event(&key_event(VK_DOWN as u16, 0, 0, 1))
            .expect("Down arrow should translate");
        assert_eq!(event.data, b"\x1b[B");
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_right_arrow() {
        use winapi::um::winuser::VK_RIGHT;
        let event = translate_console_key_event(&key_event(VK_RIGHT as u16, 0, 0, 1))
            .expect("Right arrow should translate");
        assert_eq!(event.data, b"\x1b[C");
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_left_arrow() {
        use winapi::um::winuser::VK_LEFT;
        let event = translate_console_key_event(&key_event(VK_LEFT as u16, 0, 0, 1))
            .expect("Left arrow should translate");
        assert_eq!(event.data, b"\x1b[D");
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_unknown_vk_no_unicode_returns_none() {
        // Unknown VK with no unicode char → should return None
        let result = translate_console_key_event(&key_event(0xFF, 0, 0, 1));
        assert!(result.is_none());
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_alt_escape_prefix() {
        // Alt+letter should prepend ESC byte to the character
        let event =
            translate_console_key_event(&key_event(b'A' as u16, 'a' as u16, LEFT_ALT_PRESSED, 1))
                .expect("Alt+a should translate");
        assert_eq!(event.data, b"\x1ba");
        assert!(event.alt);
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_ctrl_a() {
        let event =
            translate_console_key_event(&key_event(b'A' as u16, 'a' as u16, LEFT_CTRL_PRESSED, 1))
                .expect("Ctrl+A should translate");
        assert_eq!(event.data, [0x01]); // SOH
        assert!(event.ctrl);
    }

    #[test]
    #[cfg(windows)]
    fn translate_console_key_ctrl_z() {
        let event =
            translate_console_key_event(&key_event(b'Z' as u16, 'z' as u16, LEFT_CTRL_PRESSED, 1))
                .expect("Ctrl+Z should translate");
        assert_eq!(event.data, [0x1A]); // SUB
        assert!(event.ctrl);
    }

    // ── NativeSignalBool tests (no PyO3 needed) ──

    #[test]
    fn signal_bool_default_false() {
        let sb = NativeSignalBool::new(false);
        assert!(!sb.load_nolock());
    }

    #[test]
    fn signal_bool_default_true() {
        let sb = NativeSignalBool::new(true);
        assert!(sb.load_nolock());
    }

    #[test]
    fn signal_bool_store_and_load() {
        let sb = NativeSignalBool::new(false);
        sb.store_locked(true);
        assert!(sb.load_nolock());
        sb.store_locked(false);
        assert!(!sb.load_nolock());
    }

    #[test]
    fn signal_bool_compare_and_swap_success() {
        let sb = NativeSignalBool::new(false);
        assert!(sb.compare_and_swap_locked(false, true));
        assert!(sb.load_nolock());
    }

    #[test]
    fn signal_bool_compare_and_swap_failure() {
        let sb = NativeSignalBool::new(false);
        assert!(!sb.compare_and_swap_locked(true, false));
        assert!(!sb.load_nolock());
    }

    // ── NativePtyBuffer tests (non-Python methods) ──

    #[test]
    fn pty_buffer_available_empty() {
        let buf = NativePtyBuffer::new(false, "utf-8", "replace");
        assert!(!buf.available());
    }

    #[test]
    fn pty_buffer_record_and_available() {
        let buf = NativePtyBuffer::new(false, "utf-8", "replace");
        buf.record_output(b"hello");
        assert!(buf.available());
    }

    #[test]
    fn pty_buffer_history_bytes_and_clear() {
        let buf = NativePtyBuffer::new(false, "utf-8", "replace");
        buf.record_output(b"hello");
        buf.record_output(b"world");
        assert_eq!(buf.history_bytes(), 10);
        let released = buf.clear_history();
        assert_eq!(released, 10);
        assert_eq!(buf.history_bytes(), 0);
    }

    #[test]
    fn pty_buffer_close() {
        let buf = NativePtyBuffer::new(false, "utf-8", "replace");
        buf.close();
        // After close, buffer is marked as closed
        // (no panic, graceful handling)
    }

    // ── NativePtyBuffer tests with PyO3 ──

    #[test]
    fn pty_buffer_drain_returns_recorded_chunks() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let buf = NativePtyBuffer::new(false, "utf-8", "replace");
            buf.record_output(b"chunk1");
            buf.record_output(b"chunk2");
            let drained = buf.drain(py).unwrap();
            assert_eq!(drained.len(), 2);
            assert!(!buf.available());
        });
    }

    #[test]
    fn pty_buffer_output_returns_full_history() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let buf = NativePtyBuffer::new(true, "utf-8", "replace");
            buf.record_output(b"hello ");
            buf.record_output(b"world");
            let output = buf.output(py).unwrap();
            let text: String = output.extract(py).unwrap();
            assert_eq!(text, "hello world");
        });
    }

    #[test]
    fn pty_buffer_output_since_offset() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let buf = NativePtyBuffer::new(true, "utf-8", "replace");
            buf.record_output(b"hello ");
            buf.record_output(b"world");
            let output = buf.output_since(py, 6).unwrap();
            let text: String = output.extract(py).unwrap();
            assert_eq!(text, "world");
        });
    }

    #[test]
    fn pty_buffer_read_non_blocking_empty() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let buf = NativePtyBuffer::new(false, "utf-8", "replace");
            let result = buf.read_non_blocking(py).unwrap();
            assert!(result.is_none());
        });
    }

    #[test]
    fn pty_buffer_read_non_blocking_with_data() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let buf = NativePtyBuffer::new(false, "utf-8", "replace");
            buf.record_output(b"data");
            let result = buf.read_non_blocking(py).unwrap();
            assert!(result.is_some());
        });
    }

    #[test]
    fn pty_buffer_read_closed_returns_error() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let buf = NativePtyBuffer::new(false, "utf-8", "replace");
            buf.close();
            let result = buf.read_non_blocking(py);
            assert!(result.is_err());
        });
    }

    #[test]
    fn pty_buffer_read_with_timeout() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let buf = NativePtyBuffer::new(false, "utf-8", "replace");
            let result = buf.read(py, Some(0.05));
            // Should timeout since no data
            assert!(result.is_err());
        });
    }

    #[test]
    fn pty_buffer_text_mode_decodes_utf8() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let buf = NativePtyBuffer::new(true, "utf-8", "replace");
            buf.record_output("héllo".as_bytes());
            let result = buf.read_non_blocking(py).unwrap().unwrap();
            let text: String = result.extract(py).unwrap();
            assert_eq!(text, "héllo");
        });
    }

    #[test]
    fn pty_buffer_bytes_mode_returns_bytes() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let buf = NativePtyBuffer::new(false, "utf-8", "replace");
            buf.record_output(b"\xff\xfe");
            let result = buf.read_non_blocking(py).unwrap().unwrap();
            let bytes: Vec<u8> = result.extract(py).unwrap();
            assert_eq!(bytes, vec![0xff, 0xfe]);
        });
    }

    // ── NativeIdleDetector tests (requires PyO3) ──

    fn make_idle_detector(
        py: pyo3::Python<'_>,
        timeout_seconds: f64,
        enabled: bool,
        initial_idle_for: f64,
    ) -> NativeIdleDetector {
        let signal = pyo3::Py::new(py, NativeSignalBool::new(enabled)).unwrap();
        NativeIdleDetector::new(
            py,
            timeout_seconds,
            0.0,  // stability_window_seconds
            0.01, // sample_interval_seconds
            signal,
            true, // reset_on_input
            true, // reset_on_output
            true, // count_control_churn_as_output
            initial_idle_for,
        )
    }

    #[test]
    fn idle_detector_mark_exit_returns_process_exit() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let det = make_idle_detector(py, 10.0, true, 0.0);
            det.mark_exit(42, false);
            let (triggered, reason, _idle_for, returncode) = det.wait(py, Some(1.0));
            assert!(!triggered);
            assert_eq!(reason, "process_exit");
            assert_eq!(returncode, Some(42));
        });
    }

    #[test]
    fn idle_detector_mark_exit_interrupted() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let det = make_idle_detector(py, 10.0, true, 0.0);
            det.mark_exit(1, true);
            let (triggered, reason, _idle_for, returncode) = det.wait(py, Some(1.0));
            assert!(!triggered);
            assert_eq!(reason, "interrupt");
            assert_eq!(returncode, Some(1));
        });
    }

    #[test]
    fn idle_detector_timeout_when_not_idle() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let det = make_idle_detector(py, 10.0, true, 0.0);
            let (triggered, reason, _idle_for, returncode) = det.wait(py, Some(0.05));
            assert!(!triggered);
            assert_eq!(reason, "timeout");
            assert!(returncode.is_none());
        });
    }

    #[test]
    fn idle_detector_triggers_when_already_idle() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            // initial_idle_for=1.0 means it thinks it's been idle for 1 second
            // timeout_seconds=0.5 means 0.5s idle triggers
            let det = make_idle_detector(py, 0.5, true, 1.0);
            let (triggered, reason, _idle_for, _returncode) = det.wait(py, Some(1.0));
            assert!(triggered);
            assert_eq!(reason, "idle_timeout");
        });
    }

    #[test]
    fn idle_detector_disabled_does_not_trigger() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let det = make_idle_detector(py, 0.01, false, 1.0);
            let (triggered, reason, _idle_for, _returncode) = det.wait(py, Some(0.1));
            assert!(!triggered);
            assert_eq!(reason, "timeout");
        });
    }

    #[test]
    fn idle_detector_record_input_resets_idle() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let det = make_idle_detector(py, 0.5, true, 1.0);
            // Recording input should reset the idle timer
            det.record_input(5);
            // Now it should NOT trigger immediately since we just reset
            let (triggered, reason, _idle_for, _returncode) = det.wait(py, Some(0.05));
            assert!(!triggered);
            assert_eq!(reason, "timeout");
        });
    }

    #[test]
    fn idle_detector_record_output_resets_idle() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let det = make_idle_detector(py, 0.5, true, 1.0);
            // Recording visible output should reset idle timer
            det.record_output(b"visible output");
            let (triggered, reason, _idle_for, _returncode) = det.wait(py, Some(0.05));
            assert!(!triggered);
            assert_eq!(reason, "timeout");
        });
    }

    #[test]
    fn idle_detector_control_churn_only_no_reset_when_not_counted() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let signal = pyo3::Py::new(py, NativeSignalBool::new(true)).unwrap();
            let det = NativeIdleDetector::new(
                py, 0.05, 0.0, 0.01, signal, true, true,
                false, // count_control_churn_as_output = false
                1.0,   // already idle for 1s
            );
            // Output only ANSI escape (no visible content)
            det.record_output(b"\x1b[31m");
            // Should still trigger because control churn doesn't count
            let (triggered, reason, _idle_for, _returncode) = det.wait(py, Some(0.5));
            assert!(triggered);
            assert_eq!(reason, "idle_timeout");
        });
    }

    // ── Process tracking tests (requires PyO3) ──

    #[test]
    fn process_registry_register_list_unregister() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let test_pid = 99999u32;
            // Register
            native_register_process(test_pid, "test", "test-command", None).unwrap();
            // List
            let list = native_list_active_processes();
            let found = list.iter().any(|(pid, _, _, _, _)| *pid == test_pid);
            assert!(found, "registered pid should appear in active list");
            // Unregister
            native_unregister_process(test_pid).unwrap();
            let list = native_list_active_processes();
            let found = list.iter().any(|(pid, _, _, _, _)| *pid == test_pid);
            assert!(!found, "unregistered pid should not appear in active list");
        });
    }

    // ── NativeProcessMetrics tests (requires PyO3) ──

    #[test]
    fn process_metrics_sample_current_process() {
        let pid = std::process::id();
        let metrics = NativeProcessMetrics::new(pid);
        metrics.prime();
        let (exists, _cpu, _disk, _extra) = metrics.sample();
        assert!(exists, "current process should exist");
    }

    #[test]
    fn process_metrics_nonexistent_process() {
        let metrics = NativeProcessMetrics::new(99999999);
        metrics.prime();
        let (exists, _cpu, _disk, _extra) = metrics.sample();
        assert!(!exists, "nonexistent pid should not exist");
    }

    // ── portable_exit_code tests ──

    #[test]
    fn portable_exit_code_normal_exit_zero() {
        let status =
            running_process_core::pty::reexports::portable_pty::ExitStatus::with_exit_code(0);
        assert_eq!(core_pty::portable_exit_code(status), 0);
    }

    #[test]
    fn portable_exit_code_normal_exit_nonzero() {
        let status =
            running_process_core::pty::reexports::portable_pty::ExitStatus::with_exit_code(42);
        assert_eq!(core_pty::portable_exit_code(status), 42);
    }

    // ── record_pty_input_metrics tests ──

    #[test]
    fn record_pty_input_metrics_basic() {
        let input_bytes = Arc::new(AtomicUsize::new(0));
        let newline_events = Arc::new(AtomicUsize::new(0));
        let submit_events = Arc::new(AtomicUsize::new(0));

        core_pty::record_pty_input_metrics(
            &input_bytes,
            &newline_events,
            &submit_events,
            b"hello",
            false,
        );

        assert_eq!(input_bytes.load(Ordering::Acquire), 5);
        assert_eq!(newline_events.load(Ordering::Acquire), 0);
        assert_eq!(submit_events.load(Ordering::Acquire), 0);
    }

    #[test]
    fn record_pty_input_metrics_with_newline() {
        let input_bytes = Arc::new(AtomicUsize::new(0));
        let newline_events = Arc::new(AtomicUsize::new(0));
        let submit_events = Arc::new(AtomicUsize::new(0));

        core_pty::record_pty_input_metrics(
            &input_bytes,
            &newline_events,
            &submit_events,
            b"hello\n",
            false,
        );

        assert_eq!(input_bytes.load(Ordering::Acquire), 6);
        assert_eq!(newline_events.load(Ordering::Acquire), 1);
        assert_eq!(submit_events.load(Ordering::Acquire), 0);
    }

    #[test]
    fn record_pty_input_metrics_with_submit() {
        let input_bytes = Arc::new(AtomicUsize::new(0));
        let newline_events = Arc::new(AtomicUsize::new(0));
        let submit_events = Arc::new(AtomicUsize::new(0));

        core_pty::record_pty_input_metrics(
            &input_bytes,
            &newline_events,
            &submit_events,
            b"\r",
            true,
        );

        assert_eq!(input_bytes.load(Ordering::Acquire), 1);
        assert_eq!(newline_events.load(Ordering::Acquire), 1);
        assert_eq!(submit_events.load(Ordering::Acquire), 1);
    }

    #[test]
    fn record_pty_input_metrics_accumulates() {
        let input_bytes = Arc::new(AtomicUsize::new(0));
        let newline_events = Arc::new(AtomicUsize::new(0));
        let submit_events = Arc::new(AtomicUsize::new(0));

        core_pty::record_pty_input_metrics(
            &input_bytes,
            &newline_events,
            &submit_events,
            b"ab",
            false,
        );
        core_pty::record_pty_input_metrics(
            &input_bytes,
            &newline_events,
            &submit_events,
            b"cd\n",
            true,
        );

        assert_eq!(input_bytes.load(Ordering::Acquire), 5);
        assert_eq!(newline_events.load(Ordering::Acquire), 1);
        assert_eq!(submit_events.load(Ordering::Acquire), 1);
    }

    // ── store_pty_returncode tests ──

    #[test]
    fn store_pty_returncode_sets_value() {
        let returncode = Arc::new(Mutex::new(None));
        core_pty::store_pty_returncode(&returncode, 42);
        assert_eq!(*returncode.lock().unwrap(), Some(42));
    }

    #[test]
    fn store_pty_returncode_overwrites() {
        let returncode = Arc::new(Mutex::new(Some(1)));
        core_pty::store_pty_returncode(&returncode, 0);
        assert_eq!(*returncode.lock().unwrap(), Some(0));
    }

    // ── write_pty_input error path tests ──

    #[test]
    fn write_pty_input_not_connected() {
        let handles: Arc<Mutex<Option<NativePtyHandles>>> = Arc::new(Mutex::new(None));
        let result = core_pty::write_pty_input(&handles, b"hello");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotConnected);
    }

    // ── poll_pty_process tests ──

    #[test]
    fn poll_pty_process_no_handles_returns_stored_code() {
        let handles: Arc<Mutex<Option<NativePtyHandles>>> = Arc::new(Mutex::new(None));
        let returncode = Arc::new(Mutex::new(Some(42)));
        let result = core_pty::poll_pty_process(&handles, &returncode).unwrap();
        assert_eq!(result, Some(42));
    }

    #[test]
    fn poll_pty_process_no_handles_no_code() {
        let handles: Arc<Mutex<Option<NativePtyHandles>>> = Arc::new(Mutex::new(None));
        let returncode = Arc::new(Mutex::new(None));
        let result = core_pty::poll_pty_process(&handles, &returncode).unwrap();
        assert_eq!(result, None);
    }

    // ── descendant_pids tests ──

    #[test]
    fn descendant_pids_returns_empty_for_unknown_pid() {
        let system = System::new();
        let pid = system_pid(99999999);
        let descendants = descendant_pids(&system, pid);
        assert!(descendants.is_empty());
    }

    // ── unix_now_seconds tests ──

    #[test]
    fn unix_now_seconds_returns_positive() {
        let now = unix_now_seconds();
        assert!(now > 0.0, "unix timestamp should be positive");
    }

    // ── same_process_identity tests ──

    #[test]
    fn same_process_identity_nonexistent_pid() {
        assert!(!same_process_identity(99999999, 0.0, 1.0));
    }

    // ── tracked_process_db_path tests ──

    #[test]
    fn tracked_process_db_path_returns_ok() {
        let path = tracked_process_db_path();
        assert!(path.is_ok());
        let path = path.unwrap();
        assert!(
            path.to_string_lossy().contains("tracked-pids.sqlite3"),
            "path should contain expected filename: {:?}",
            path
        );
    }

    // ── command_builder_from_argv tests ──

    #[test]
    fn command_builder_from_argv_single_arg() {
        let argv = vec!["echo".to_string()];
        let _cmd = core_pty::command_builder_from_argv(&argv);
        // Just ensure it doesn't panic
    }

    #[test]
    fn command_builder_from_argv_multi_args() {
        let argv = vec!["echo".to_string(), "hello".to_string(), "world".to_string()];
        let _cmd = core_pty::command_builder_from_argv(&argv);
        // Just ensure it doesn't panic
    }

    // ── process_err_to_py tests ──

    #[test]
    fn process_err_to_py_timeout() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let err = process_err_to_py(ProcessError::Timeout);
            assert!(err.is_instance_of::<pyo3::exceptions::PyTimeoutError>(py));
        });
    }

    // ── kill_process_tree_impl tests ──

    #[test]
    fn kill_process_tree_nonexistent_pid_no_panic() {
        // Should not panic when given a PID that doesn't exist
        kill_process_tree_impl(99999999, 0.1);
    }

    // ── NativeIdleDetector additional tests ──

    #[test]
    fn idle_detector_record_input_zero_bytes_no_reset() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let det = make_idle_detector(py, 0.05, true, 1.0);
            // Recording 0 bytes should NOT reset idle timer
            det.record_input(0);
            let (triggered, reason, _idle_for, _returncode) = det.wait(py, Some(0.5));
            assert!(triggered);
            assert_eq!(reason, "idle_timeout");
        });
    }

    #[test]
    fn idle_detector_record_output_empty_no_reset() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let det = make_idle_detector(py, 0.05, true, 1.0);
            // Recording empty output should NOT reset idle timer
            det.record_output(b"");
            let (triggered, reason, _idle_for, _returncode) = det.wait(py, Some(0.5));
            assert!(triggered);
            assert_eq!(reason, "idle_timeout");
        });
    }

    #[test]
    fn idle_detector_enabled_getter_and_setter() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let det = make_idle_detector(py, 1.0, true, 0.0);
            assert!(det.enabled());
            det.set_enabled(false);
            assert!(!det.enabled());
            det.set_enabled(true);
            assert!(det.enabled());
        });
    }

    // ── NativePtyBuffer additional tests ──

    #[test]
    fn pty_buffer_multiple_record_and_drain() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let buf = NativePtyBuffer::new(false, "utf-8", "replace");
            buf.record_output(b"a");
            buf.record_output(b"b");
            buf.record_output(b"c");
            let drained = buf.drain(py).unwrap();
            assert_eq!(drained.len(), 3);
            assert!(!buf.available());
            // history should still be available
            assert_eq!(buf.history_bytes(), 3);
        });
    }

    #[test]
    fn pty_buffer_output_since_beyond_length() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let buf = NativePtyBuffer::new(true, "utf-8", "replace");
            buf.record_output(b"hi");
            let output = buf.output_since(py, 999).unwrap();
            let text: String = output.extract(py).unwrap();
            assert_eq!(text, "");
        });
    }

    #[test]
    fn pty_buffer_clear_history_returns_correct_bytes() {
        let buf = NativePtyBuffer::new(false, "utf-8", "replace");
        buf.record_output(b"hello");
        buf.record_output(b"world");
        assert_eq!(buf.history_bytes(), 10);
        let released = buf.clear_history();
        assert_eq!(released, 10);
        assert_eq!(buf.history_bytes(), 0);
        // Record more after clear
        buf.record_output(b"new");
        assert_eq!(buf.history_bytes(), 3);
    }

    // ── NativeSignalBool additional tests ──

    #[test]
    fn signal_bool_concurrent_access() {
        let sb = NativeSignalBool::new(false);
        let sb_clone = sb.clone();

        let handle = std::thread::spawn(move || {
            sb_clone.store_locked(true);
        });
        handle.join().unwrap();
        assert!(sb.load_nolock());
    }

    // ── control_churn_bytes additional edge cases ──

    #[test]
    fn control_churn_bytes_escape_then_non_bracket() {
        // ESC followed by non-bracket character: only ESC itself is churn
        assert_eq!(core_pty::control_churn_bytes(b"\x1bO"), 1);
    }

    #[test]
    fn control_churn_bytes_incomplete_csi() {
        // ESC [ without terminator - counts entire remainder as churn
        assert_eq!(core_pty::control_churn_bytes(b"\x1b[123"), 5);
    }

    #[test]
    fn control_churn_bytes_multiple_sequences() {
        // Two complete CSI sequences
        assert_eq!(core_pty::control_churn_bytes(b"\x1b[H\x1b[2J"), 7);
    }

    // ── Windows-specific additional tests ──

    #[cfg(windows)]
    mod windows_payload_tests {
        use super::*;

        #[test]
        fn windows_terminal_input_payload_mixed_line_endings() {
            let result = core_pty::windows_terminal_input_payload(b"a\nb\r\nc\rd");
            assert_eq!(result, b"a\rb\r\nc\rd");
        }

        #[test]
        fn windows_terminal_input_payload_consecutive_lf() {
            let result = core_pty::windows_terminal_input_payload(b"\n\n");
            assert_eq!(result, b"\r\r");
        }

        #[test]
        fn windows_terminal_input_payload_empty() {
            let result = core_pty::windows_terminal_input_payload(b"");
            assert!(result.is_empty());
        }

        #[test]
        fn windows_terminal_input_payload_no_line_endings() {
            let result = core_pty::windows_terminal_input_payload(b"hello world");
            assert_eq!(result, b"hello world");
        }

        #[test]
        fn format_terminal_input_bytes_single() {
            assert_eq!(format_terminal_input_bytes(&[0x0D]), "[0d]");
        }

        #[test]
        fn native_terminal_input_mode_preserves_other_flags() {
            // Pass a mode with an unrelated flag set
            let custom_flag = 0x0100; // some arbitrary flag
            let result = native_terminal_input_mode(custom_flag);
            // The custom flag should be preserved
            assert_ne!(result & custom_flag, 0);
        }
    }

    // ── Process registry additional tests ──

    #[test]
    fn process_registry_register_with_cwd() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let test_pid = 99998u32;
            native_register_process(test_pid, "test", "test-cmd", Some("/tmp/test".to_string()))
                .unwrap();
            let list = native_list_active_processes();
            let entry = list.iter().find(|(pid, _, _, _, _)| *pid == test_pid);
            assert!(entry.is_some());
            let (_, kind, cmd, cwd, _) = entry.unwrap();
            assert_eq!(kind, "test");
            assert_eq!(cmd, "test-cmd");
            assert_eq!(cwd.as_deref(), Some("/tmp/test"));
            native_unregister_process(test_pid).unwrap();
        });
    }

    #[test]
    fn process_registry_double_register_overwrites() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let test_pid = 99997u32;
            native_register_process(test_pid, "first", "cmd1", None).unwrap();
            native_register_process(test_pid, "second", "cmd2", None).unwrap();
            let list = native_list_active_processes();
            let entries: Vec<_> = list
                .iter()
                .filter(|(pid, _, _, _, _)| *pid == test_pid)
                .collect();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].1, "second");
            native_unregister_process(test_pid).unwrap();
        });
    }

    #[test]
    fn process_registry_unregister_nonexistent_no_error() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            // Should not error when unregistering a PID that doesn't exist
            let result = native_unregister_process(99996);
            assert!(result.is_ok());
        });
    }

    // ── list_tracked_processes tests ──

    #[test]
    fn list_tracked_processes_returns_ok() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let result = list_tracked_processes();
            assert!(result.is_ok());
        });
    }

    // ══════════════════════════════════════════════════════════════
    // Iteration 2: Additional coverage tests
    // ══════════════════════════════════════════════════════════════

    // ── is_ignorable_process_control_error additional tests ──

    #[test]
    fn non_ignorable_error_connection_refused() {
        let err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        assert!(!is_ignorable_process_control_error(&err));
    }

    // ── to_py_err tests ──

    #[test]
    fn to_py_err_creates_runtime_error() {
        pyo3::prepare_freethreaded_python();
        let err = to_py_err("test error message");
        assert!(err.to_string().contains("test error message"));
    }

    // ── process_err_to_py tests ──

    #[test]
    fn process_err_to_py_timeout_is_timeout_error() {
        pyo3::prepare_freethreaded_python();
        let err = process_err_to_py(running_process_core::ProcessError::Timeout);
        pyo3::Python::with_gil(|py| {
            assert!(err.is_instance_of::<pyo3::exceptions::PyTimeoutError>(py));
        });
    }

    #[test]
    fn process_err_to_py_not_running_is_runtime_error() {
        pyo3::prepare_freethreaded_python();
        let err = process_err_to_py(running_process_core::ProcessError::NotRunning);
        pyo3::Python::with_gil(|py| {
            assert!(err.is_instance_of::<pyo3::exceptions::PyRuntimeError>(py));
        });
    }

    // ── input_contains_newline tests ──

    #[test]
    fn input_contains_newline_with_cr() {
        assert!(core_pty::input_contains_newline(b"hello\rworld"));
    }

    #[test]
    fn input_contains_newline_with_lf() {
        assert!(core_pty::input_contains_newline(b"hello\nworld"));
    }

    #[test]
    fn input_contains_newline_with_crlf() {
        assert!(core_pty::input_contains_newline(b"hello\r\nworld"));
    }

    #[test]
    fn input_contains_newline_without_newline() {
        assert!(!core_pty::input_contains_newline(b"hello world"));
    }

    // ── control_churn_bytes additional tests (iter2) ──

    #[test]
    fn control_churn_bytes_backspace() {
        assert_eq!(core_pty::control_churn_bytes(b"\x08"), 1);
    }

    #[test]
    fn control_churn_bytes_carriage_return() {
        assert_eq!(core_pty::control_churn_bytes(b"\x0D"), 1);
    }

    #[test]
    fn control_churn_bytes_delete_char() {
        assert_eq!(core_pty::control_churn_bytes(b"\x7F"), 1);
    }

    #[test]
    fn control_churn_bytes_mixed_with_text() {
        assert_eq!(core_pty::control_churn_bytes(b"hello\x0D\x1b[H"), 4);
    }

    #[test]
    fn control_churn_bytes_plain_text_no_churn() {
        assert_eq!(core_pty::control_churn_bytes(b"hello world"), 0);
    }

    // ── system_pid tests ──

    #[test]
    fn system_pid_converts_u32() {
        let pid = system_pid(12345);
        assert_eq!(pid.as_u32(), 12345);
    }

    // ── unix_now_seconds tests ──

    #[test]
    fn unix_now_seconds_is_recent() {
        let now = unix_now_seconds();
        assert!(now > 1_577_836_800.0);
    }

    // ── NativeIdleDetector: additional wait/record scenarios ──

    #[test]
    fn idle_detector_wait_idle_timeout_with_initial_idle() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let signal = pyo3::Py::new(py, NativeSignalBool::new(true)).unwrap();
            let detector =
                NativeIdleDetector::new(py, 0.01, 0.01, 0.001, signal, true, true, true, 100.0);
            let (idle, reason, _, code) = detector.wait(py, Some(1.0));
            assert!(idle);
            assert_eq!(reason, "idle_timeout");
            assert!(code.is_none());
        });
    }

    #[test]
    fn idle_detector_record_output_only_control_churn_with_flag() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let signal = pyo3::Py::new(py, NativeSignalBool::new(true)).unwrap();
            let detector =
                NativeIdleDetector::new(py, 1.0, 0.5, 0.1, signal, true, true, true, 5.0);
            let state_before = detector.core.state.lock().unwrap().last_reset_at;
            std::thread::sleep(std::time::Duration::from_millis(10));
            detector.record_output(b"\x1b[H");
            let state_after = detector.core.state.lock().unwrap().last_reset_at;
            assert!(state_after > state_before);
        });
    }

    #[test]
    fn idle_detector_record_output_only_control_churn_without_flag() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let signal = pyo3::Py::new(py, NativeSignalBool::new(true)).unwrap();
            let detector =
                NativeIdleDetector::new(py, 1.0, 0.5, 0.1, signal, true, true, false, 5.0);
            let state_before = detector.core.state.lock().unwrap().last_reset_at;
            std::thread::sleep(std::time::Duration::from_millis(10));
            detector.record_output(b"\x1b[H");
            let state_after = detector.core.state.lock().unwrap().last_reset_at;
            assert_eq!(state_before, state_after);
        });
    }

    #[test]
    fn idle_detector_record_output_not_enabled() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let signal = pyo3::Py::new(py, NativeSignalBool::new(true)).unwrap();
            let detector =
                NativeIdleDetector::new(py, 1.0, 0.5, 0.1, signal, true, false, true, 5.0);
            let state_before = detector.core.state.lock().unwrap().last_reset_at;
            std::thread::sleep(std::time::Duration::from_millis(10));
            detector.record_output(b"visible");
            let state_after = detector.core.state.lock().unwrap().last_reset_at;
            assert_eq!(state_before, state_after);
        });
    }

    #[test]
    fn idle_detector_record_input_not_enabled() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let signal = pyo3::Py::new(py, NativeSignalBool::new(true)).unwrap();
            let detector =
                NativeIdleDetector::new(py, 1.0, 0.5, 0.1, signal, false, true, true, 5.0);
            let state_before = detector.core.state.lock().unwrap().last_reset_at;
            std::thread::sleep(std::time::Duration::from_millis(10));
            detector.record_input(100);
            let state_after = detector.core.state.lock().unwrap().last_reset_at;
            assert_eq!(state_before, state_after);
        });
    }

    #[test]
    fn idle_detector_record_input_nonzero_bytes_resets() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let signal = pyo3::Py::new(py, NativeSignalBool::new(true)).unwrap();
            let detector =
                NativeIdleDetector::new(py, 1.0, 0.5, 0.1, signal, true, true, true, 5.0);
            let state_before = detector.core.state.lock().unwrap().last_reset_at;
            std::thread::sleep(std::time::Duration::from_millis(10));
            detector.record_input(100);
            let state_after = detector.core.state.lock().unwrap().last_reset_at;
            assert!(state_after > state_before);
        });
    }

    #[test]
    fn idle_detector_record_output_visible_resets() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let signal = pyo3::Py::new(py, NativeSignalBool::new(true)).unwrap();
            let detector =
                NativeIdleDetector::new(py, 1.0, 0.5, 0.1, signal, true, true, true, 5.0);
            let state_before = detector.core.state.lock().unwrap().last_reset_at;
            std::thread::sleep(std::time::Duration::from_millis(10));
            detector.record_output(b"visible output");
            let state_after = detector.core.state.lock().unwrap().last_reset_at;
            assert!(state_after > state_before);
        });
    }

    #[test]
    fn idle_detector_mark_exit_sets_returncode() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let signal = pyo3::Py::new(py, NativeSignalBool::new(true)).unwrap();
            let detector =
                NativeIdleDetector::new(py, 1.0, 0.5, 0.1, signal, true, true, true, 0.0);
            detector.mark_exit(42, false);
            let state = detector.core.state.lock().unwrap();
            assert_eq!(state.returncode, Some(42));
            assert!(!state.interrupted);
        });
    }

    // ── find_expect_match tests ──

    #[test]
    fn find_expect_match_literal_found() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let process = make_test_running_process(py);
            let result = process
                .find_expect_match("hello world", "world", None)
                .unwrap();
            assert!(result.is_some());
            let (matched, start, end, groups) = result.unwrap();
            assert_eq!(matched, "world");
            assert_eq!(start, 6);
            assert_eq!(end, 11);
            assert!(groups.is_empty());
        });
    }

    #[test]
    fn find_expect_match_literal_not_found() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let process = make_test_running_process(py);
            let result = process
                .find_expect_match("hello world", "missing", None)
                .unwrap();
            assert!(result.is_none());
        });
    }

    #[test]
    fn find_expect_match_regex_found() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let process = make_test_running_process(py);
            let re = Regex::new(r"\d+").unwrap();
            let result = process
                .find_expect_match("hello 123 world", r"\d+", Some(&re))
                .unwrap();
            assert!(result.is_some());
            let (matched, start, end, _) = result.unwrap();
            assert_eq!(matched, "123");
            assert_eq!(start, 6);
            assert_eq!(end, 9);
        });
    }

    #[test]
    fn find_expect_match_regex_with_groups() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let process = make_test_running_process(py);
            let re = Regex::new(r"(\d+) (\w+)").unwrap();
            let result = process
                .find_expect_match("hello 123 world", r"(\d+) (\w+)", Some(&re))
                .unwrap();
            assert!(result.is_some());
            let (_, _, _, groups) = result.unwrap();
            assert_eq!(groups.len(), 2);
            assert_eq!(groups[0], "123");
            assert_eq!(groups[1], "world");
        });
    }

    #[test]
    fn find_expect_match_regex_not_found() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let process = make_test_running_process(py);
            let re = Regex::new(r"\d+").unwrap();
            let result = process
                .find_expect_match("hello world", r"\d+", Some(&re))
                .unwrap();
            assert!(result.is_none());
        });
    }

    #[test]
    #[allow(clippy::invalid_regex)]
    fn find_expect_match_invalid_regex_errors() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let result = Regex::new(r"[invalid");
            assert!(result.is_err());
        });
    }

    fn make_test_running_process(py: Python<'_>) -> NativeRunningProcess {
        let cmd = pyo3::types::PyList::new(py, ["echo", "test"]).unwrap();
        NativeRunningProcess::new(
            cmd.as_any(),
            None,
            false,
            true,
            None,
            None,
            true,
            None,
            None,
            "inherit",
            "stdout",
            None,
            false,
        )
        .unwrap()
    }

    // ── parse_command tests ──

    #[test]
    fn parse_command_string_with_shell() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd = pyo3::types::PyString::new(py, "echo hello");
            let result = parse_command(cmd.as_any(), true).unwrap();
            assert!(matches!(result, CommandSpec::Shell(ref s) if s == "echo hello"));
        });
    }

    #[test]
    fn parse_command_string_without_shell_errors() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd = pyo3::types::PyString::new(py, "echo hello");
            let result = parse_command(cmd.as_any(), false);
            assert!(result.is_err());
        });
    }

    #[test]
    fn parse_command_list_without_shell() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd = pyo3::types::PyList::new(py, ["echo", "hello"]).unwrap();
            let result = parse_command(cmd.as_any(), false).unwrap();
            assert!(matches!(result, CommandSpec::Argv(ref v) if v.len() == 2));
        });
    }

    #[test]
    fn parse_command_list_with_shell_joins() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd = pyo3::types::PyList::new(py, ["echo", "hello"]).unwrap();
            let result = parse_command(cmd.as_any(), true).unwrap();
            assert!(matches!(result, CommandSpec::Shell(ref s) if s == "echo hello"));
        });
    }

    #[test]
    fn parse_command_empty_list_errors() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd = pyo3::types::PyList::empty(py);
            let result = parse_command(cmd.as_any(), false);
            assert!(result.is_err());
        });
    }

    #[test]
    fn parse_command_invalid_type_errors() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd = 42i32.into_pyobject(py).unwrap();
            let result = parse_command(cmd.as_any(), false);
            assert!(result.is_err());
        });
    }

    // ── stream_kind tests ──

    #[test]
    fn stream_kind_stdout() {
        let result = stream_kind("stdout").unwrap();
        assert_eq!(result, StreamKind::Stdout);
    }

    #[test]
    fn stream_kind_stderr() {
        let result = stream_kind("stderr").unwrap();
        assert_eq!(result, StreamKind::Stderr);
    }

    #[test]
    fn stream_kind_invalid() {
        let result = stream_kind("invalid");
        assert!(result.is_err());
    }

    // ── stdin_mode tests ──

    #[test]
    fn stdin_mode_inherit() {
        assert_eq!(stdin_mode("inherit").unwrap(), StdinMode::Inherit);
    }

    #[test]
    fn stdin_mode_piped() {
        assert_eq!(stdin_mode("piped").unwrap(), StdinMode::Piped);
    }

    #[test]
    fn stdin_mode_null() {
        assert_eq!(stdin_mode("null").unwrap(), StdinMode::Null);
    }

    #[test]
    fn stdin_mode_invalid() {
        assert!(stdin_mode("invalid").is_err());
    }

    // ── stderr_mode tests ──

    #[test]
    fn stderr_mode_stdout() {
        assert_eq!(stderr_mode("stdout").unwrap(), StderrMode::Stdout);
    }

    #[test]
    fn stderr_mode_pipe() {
        assert_eq!(stderr_mode("pipe").unwrap(), StderrMode::Pipe);
    }

    #[test]
    fn stderr_mode_invalid() {
        assert!(stderr_mode("invalid").is_err());
    }

    // ── Windows-specific additional tests (iter2) ──

    #[cfg(windows)]
    mod windows_additional_tests {
        use super::*;
        use winapi::um::winuser::VK_F1;

        // ── control_character_for_unicode tests ──

        #[test]
        fn control_char_at_sign() {
            assert_eq!(control_character_for_unicode('@' as u16), Some(0x00));
        }

        #[test]
        fn control_char_space() {
            assert_eq!(control_character_for_unicode(' ' as u16), Some(0x00));
        }

        #[test]
        fn control_char_a() {
            assert_eq!(control_character_for_unicode('a' as u16), Some(0x01));
        }

        #[test]
        fn control_char_z() {
            assert_eq!(control_character_for_unicode('z' as u16), Some(0x1A));
        }

        #[test]
        fn control_char_bracket() {
            assert_eq!(control_character_for_unicode('[' as u16), Some(0x1B));
        }

        #[test]
        fn control_char_backslash() {
            assert_eq!(control_character_for_unicode('\\' as u16), Some(0x1C));
        }

        #[test]
        fn control_char_close_bracket() {
            assert_eq!(control_character_for_unicode(']' as u16), Some(0x1D));
        }

        #[test]
        fn control_char_caret() {
            assert_eq!(control_character_for_unicode('^' as u16), Some(0x1E));
        }

        #[test]
        fn control_char_underscore() {
            assert_eq!(control_character_for_unicode('_' as u16), Some(0x1F));
        }

        #[test]
        fn control_char_digit_returns_none() {
            assert_eq!(control_character_for_unicode('0' as u16), None);
        }

        #[test]
        fn control_char_exclamation_returns_none() {
            assert_eq!(control_character_for_unicode('!' as u16), None);
        }

        // ── terminal_input_modifier_parameter tests ──

        #[test]
        fn modifier_param_no_modifiers_returns_none() {
            assert_eq!(terminal_input_modifier_parameter(false, false, false), None);
        }

        #[test]
        fn modifier_param_shift_only() {
            assert_eq!(
                terminal_input_modifier_parameter(true, false, false),
                Some(2)
            );
        }

        #[test]
        fn modifier_param_alt_only() {
            assert_eq!(
                terminal_input_modifier_parameter(false, true, false),
                Some(3)
            );
        }

        #[test]
        fn modifier_param_ctrl_only() {
            assert_eq!(
                terminal_input_modifier_parameter(false, false, true),
                Some(5)
            );
        }

        #[test]
        fn modifier_param_shift_ctrl() {
            assert_eq!(
                terminal_input_modifier_parameter(true, false, true),
                Some(6)
            );
        }

        #[test]
        fn modifier_param_shift_alt() {
            assert_eq!(
                terminal_input_modifier_parameter(true, true, false),
                Some(4)
            );
        }

        #[test]
        fn modifier_param_all_modifiers() {
            assert_eq!(terminal_input_modifier_parameter(true, true, true), Some(8));
        }

        // ── repeated_tilde_sequence tests ──

        #[test]
        fn tilde_sequence_no_modifier() {
            let result = repeated_tilde_sequence(3, None, 1);
            assert_eq!(result, b"\x1b[3~");
        }

        #[test]
        fn tilde_sequence_with_modifier() {
            let result = repeated_tilde_sequence(3, Some(2), 1);
            assert_eq!(result, b"\x1b[3;2~");
        }

        #[test]
        fn tilde_sequence_repeated() {
            let result = repeated_tilde_sequence(3, None, 3);
            assert_eq!(result, b"\x1b[3~\x1b[3~\x1b[3~");
        }

        // ── repeated_modified_sequence tests ──

        #[test]
        fn modified_sequence_no_modifier() {
            let result = repeated_modified_sequence(b"\x1b[A", None, 1);
            assert_eq!(result, b"\x1b[A");
        }

        #[test]
        fn modified_sequence_with_modifier() {
            let result = repeated_modified_sequence(b"\x1b[A", Some(2), 1);
            assert_eq!(result, b"\x1b[1;2A");
        }

        #[test]
        fn modified_sequence_repeated_with_modifier() {
            let result = repeated_modified_sequence(b"\x1b[A", Some(5), 2);
            assert_eq!(result, b"\x1b[1;5A\x1b[1;5A");
        }

        // ── format_terminal_input_bytes tests ──

        #[test]
        fn format_bytes_empty() {
            assert_eq!(format_terminal_input_bytes(&[]), "[]");
        }

        #[test]
        fn format_bytes_multiple() {
            assert_eq!(
                format_terminal_input_bytes(&[0x1B, 0x5B, 0x41]),
                "[1b 5b 41]"
            );
        }

        // ── native_terminal_input_trace_target tests ──

        #[test]
        fn trace_target_empty_env_returns_none() {
            std::env::remove_var(NATIVE_TERMINAL_INPUT_TRACE_PATH_ENV);
            assert!(native_terminal_input_trace_target().is_none());
        }

        #[test]
        fn trace_target_whitespace_env_returns_none() {
            std::env::set_var(NATIVE_TERMINAL_INPUT_TRACE_PATH_ENV, "   ");
            assert!(native_terminal_input_trace_target().is_none());
            std::env::remove_var(NATIVE_TERMINAL_INPUT_TRACE_PATH_ENV);
        }

        #[test]
        fn trace_target_valid_env_returns_value() {
            std::env::set_var(NATIVE_TERMINAL_INPUT_TRACE_PATH_ENV, "/tmp/trace.log");
            let result = native_terminal_input_trace_target();
            assert_eq!(result, Some("/tmp/trace.log".to_string()));
            std::env::remove_var(NATIVE_TERMINAL_INPUT_TRACE_PATH_ENV);
        }

        // ── translate_console_key_event: key-up ignored ──

        #[test]
        fn translate_key_up_event_returns_none() {
            let mut event: KEY_EVENT_RECORD = unsafe { std::mem::zeroed() };
            event.bKeyDown = 0;
            event.wVirtualKeyCode = VK_RETURN as u16;
            let result = translate_console_key_event(&event);
            assert!(result.is_none());
        }

        // ── translate: F1 returns None (unknown key) ──

        #[test]
        fn translate_f1_key_returns_none() {
            let event = key_event(VK_F1 as u16, 0, 0, 1);
            let result = translate_console_key_event(&event);
            assert!(result.is_none());
        }

        // ── translate: alt prefix ──

        #[test]
        fn translate_alt_a_has_escape_prefix() {
            let event = key_event('a' as u16, 'a' as u16, LEFT_ALT_PRESSED, 1);
            let result = translate_console_key_event(&event).unwrap();
            assert!(result.data.starts_with(b"\x1b"));
            assert!(result.alt);
        }

        // ── translate: Ctrl+character ──

        #[test]
        fn translate_ctrl_c_produces_etx() {
            let event = key_event('C' as u16, 'c' as u16, LEFT_CTRL_PRESSED, 1);
            let result = translate_console_key_event(&event).unwrap();
            assert_eq!(result.data, &[0x03]);
            assert!(result.ctrl);
        }
    }

    // ── NativeTerminalInput tests ──

    #[test]
    fn terminal_input_new_starts_closed() {
        let input = NativeTerminalInput::new();
        assert!(!input.capturing());
        let state = input.inner.state.lock().unwrap();
        assert!(state.closed);
        assert!(state.events.is_empty());
    }

    #[test]
    fn terminal_input_available_false_when_empty() {
        let input = NativeTerminalInput::new();
        assert!(!input.available());
    }

    #[test]
    fn terminal_input_next_event_none_when_empty() {
        let input = NativeTerminalInput::new();
        assert!(input.inner.next_event().is_none());
    }

    #[test]
    fn terminal_input_inject_and_consume_event() {
        let input = NativeTerminalInput::new();
        {
            let mut state = input.inner.state.lock().unwrap();
            state.events.push_back(TerminalInputEventRecord {
                data: b"test".to_vec(),
                submit: false,
                shift: false,
                ctrl: false,
                alt: false,
                virtual_key_code: 0,
                repeat_count: 1,
            });
        }
        assert!(input.available());
        let event = input.inner.next_event().unwrap();
        assert_eq!(event.data, b"test");
        assert!(!input.available());
    }

    #[test]
    #[cfg(not(windows))]
    fn terminal_input_start_errors_on_non_windows() {
        pyo3::prepare_freethreaded_python();
        let input = NativeTerminalInput::new();
        let result = input.start();
        assert!(result.is_err());
    }

    // ── NativeTerminalInputEvent __repr__ ──

    #[test]
    fn terminal_input_event_repr() {
        let event = NativeTerminalInputEvent {
            data: vec![0x0D],
            submit: true,
            shift: false,
            ctrl: false,
            alt: false,
            virtual_key_code: 13,
            repeat_count: 1,
        };
        let repr = event.__repr__();
        assert!(repr.contains("submit=true"));
        assert!(repr.contains("virtual_key_code=13"));
    }

    // ── tracked_process_db_path ──

    #[test]
    fn tracked_process_db_path_with_env() {
        pyo3::prepare_freethreaded_python();
        std::env::set_var("RUNNING_PROCESS_PID_DB", "/custom/path/db.sqlite3");
        let result = tracked_process_db_path().unwrap();
        assert_eq!(result, std::path::PathBuf::from("/custom/path/db.sqlite3"));
        std::env::remove_var("RUNNING_PROCESS_PID_DB");
    }

    #[test]
    fn tracked_process_db_path_empty_env_falls_back() {
        pyo3::prepare_freethreaded_python();
        std::env::set_var("RUNNING_PROCESS_PID_DB", "   ");
        let result = tracked_process_db_path().unwrap();
        assert!(!result.to_str().unwrap().trim().is_empty());
        std::env::remove_var("RUNNING_PROCESS_PID_DB");
    }

    // ── NativePtyProcess: start_terminal_input_relay on non-windows ──

    // Terminal input relay tests are now tested through the Python wrapper
    // since the relay logic lives in the py crate, not in core.

    // ── NativeProcessMetrics ──

    #[test]
    fn process_metrics_sample_nonexistent_pid() {
        pyo3::prepare_freethreaded_python();
        let metrics = NativeProcessMetrics::new(999999);
        let (alive, cpu, io, _) = metrics.sample();
        assert!(!alive);
        assert_eq!(cpu, 0.0);
        assert_eq!(io, 0);
    }

    #[test]
    fn process_metrics_prime_no_panic() {
        pyo3::prepare_freethreaded_python();
        let metrics = NativeProcessMetrics::new(999999);
        metrics.prime();
    }

    // ── ActiveProcessRecord ──

    #[test]
    fn active_process_record_clone() {
        let record = ActiveProcessRecord {
            pid: 1234,
            kind: "test".to_string(),
            command: "echo".to_string(),
            cwd: Some("/tmp".to_string()),
            started_at: 1000.0,
        };
        let cloned = record.clone();
        assert_eq!(cloned.pid, 1234);
        assert_eq!(cloned.kind, "test");
        assert_eq!(cloned.command, "echo");
        assert_eq!(cloned.cwd, Some("/tmp".to_string()));
    }

    // ── NativePtyProcess: empty argv errors ──

    #[test]
    fn pty_process_empty_argv_errors() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let result = CoreNativePtyProcess::new(vec![], None, None, 24, 80, None);
            assert!(result.is_err());
        });
    }

    // ── NativePtyProcess: start already started errors ──

    #[test]
    #[cfg(not(windows))]
    fn pty_process_start_already_started_errors() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec![
                "python".to_string(),
                "-c".to_string(),
                "import time; time.sleep(0.1)".to_string(),
            ];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            let result = process.start_impl();
            assert!(result.is_err());
            let _ = process.close_impl();
        });
    }

    // ── Iteration 3: NativePtyBuffer additional tests ──

    #[test]
    fn pty_buffer_new_defaults() {
        let buf = NativePtyBuffer::new(false, "utf-8", "replace");
        assert!(!buf.available());
        assert_eq!(buf.history_bytes(), 0);
    }

    #[test]
    fn pty_buffer_record_output_makes_available() {
        let buf = NativePtyBuffer::new(false, "utf-8", "replace");
        buf.record_output(b"hello");
        assert!(buf.available());
    }

    #[test]
    fn pty_buffer_history_bytes_accumulates() {
        let buf = NativePtyBuffer::new(false, "utf-8", "replace");
        buf.record_output(b"hello");
        assert_eq!(buf.history_bytes(), 5);
        buf.record_output(b" world");
        assert_eq!(buf.history_bytes(), 11);
    }

    #[test]
    fn pty_buffer_clear_history_resets_to_zero() {
        let buf = NativePtyBuffer::new(false, "utf-8", "replace");
        buf.record_output(b"data");
        let released = buf.clear_history();
        assert_eq!(released, 4);
        assert_eq!(buf.history_bytes(), 0);
    }

    #[test]
    fn pty_buffer_close_sets_closed_flag() {
        let buf = NativePtyBuffer::new(false, "utf-8", "replace");
        buf.close();
        let state = buf.state.lock().unwrap();
        assert!(state.closed);
    }

    #[test]
    fn pty_buffer_record_multiple_chunks_all_available() {
        let buf = NativePtyBuffer::new(false, "utf-8", "replace");
        buf.record_output(b"a");
        buf.record_output(b"bb");
        buf.record_output(b"ccc");
        assert_eq!(buf.history_bytes(), 6);
        let state = buf.state.lock().unwrap();
        assert_eq!(state.chunks.len(), 3);
    }

    // ── Iteration 3: PTY Process Integration Tests ──

    #[test]
    fn pty_process_pid_none_before_start() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            assert!(process.pid().unwrap().is_none());
        });
    }

    #[test]
    #[cfg(not(windows))]
    fn pty_process_lifecycle_start_wait_close() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec![
                "python".to_string(),
                "-c".to_string(),
                "print('hello')".to_string(),
            ];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            assert!(process.pid().unwrap().is_some());
            let code = process.wait_impl(Some(10.0)).unwrap();
            assert_eq!(code, 0);
            let _ = process.close_impl();
        });
    }

    #[test]
    #[cfg(not(windows))]
    fn pty_process_poll_none_while_running() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec![
                "python".to_string(),
                "-c".to_string(),
                "import time; time.sleep(5)".to_string(),
            ];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            assert!(
                core_pty::poll_pty_process(&process.handles, &process.returncode)
                    .unwrap()
                    .is_none()
            );
            let _ = process.close_impl();
        });
    }

    #[test]
    #[cfg(not(windows))]
    fn pty_process_nonzero_exit_code() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec![
                "python".to_string(),
                "-c".to_string(),
                "import sys; sys.exit(42)".to_string(),
            ];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            let code = process.wait_impl(Some(10.0)).unwrap();
            assert_eq!(code, 42);
            let _ = process.close_impl();
        });
    }

    #[test]
    fn pty_process_write_before_start_errors() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            assert!(process.write_impl(b"test", false).is_err());
        });
    }

    #[test]
    #[cfg(not(windows))]
    fn pty_process_input_metrics_tracked() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec![
                "python".to_string(),
                "-c".to_string(),
                "import time; time.sleep(2)".to_string(),
            ];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            assert_eq!(process.pty_input_bytes_total(), 0);
            let _ = process.write_impl(b"hello\n", false);
            assert_eq!(process.pty_input_bytes_total(), 6);
            assert_eq!(process.pty_newline_events_total(), 1);
            let _ = process.write_impl(b"x", true);
            assert_eq!(process.pty_submit_events_total(), 1);
            let _ = process.close_impl();
        });
    }

    #[test]
    #[cfg(not(windows))]
    fn pty_process_resize_while_running() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec![
                "python".to_string(),
                "-c".to_string(),
                "import time; time.sleep(2)".to_string(),
            ];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            assert!(process.resize_impl(40, 120).is_ok());
            let _ = process.close_impl();
        });
    }

    #[test]
    #[cfg(not(windows))]
    fn pty_process_kill_running_process() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec![
                "python".to_string(),
                "-c".to_string(),
                "import time; time.sleep(0.1)".to_string(),
            ];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            assert!(process.kill_impl().is_ok());
        });
    }

    #[test]
    #[cfg(not(windows))]
    fn pty_process_terminate_running_process() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec![
                "python".to_string(),
                "-c".to_string(),
                "import time; time.sleep(0.1)".to_string(),
            ];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            assert!(process.terminate_impl().is_ok());
            let _ = process.close_impl();
        });
    }

    #[test]
    #[cfg(not(windows))]
    fn pty_process_close_already_closed_is_noop() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            let _ = process.wait_impl(Some(10.0));
            let _ = process.close_impl();
            assert!(process.close_impl().is_ok());
        });
    }

    #[test]
    #[cfg(not(windows))]
    fn pty_process_wait_timeout_errors() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec![
                "python".to_string(),
                "-c".to_string(),
                "import time; time.sleep(10)".to_string(),
            ];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            assert!(process.wait_impl(Some(0.1)).is_err());
            let _ = process.close_impl();
        });
    }

    #[test]
    fn pty_process_send_interrupt_before_start_errors() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            assert!(process.send_interrupt_impl().is_err());
        });
    }

    #[test]
    fn pty_process_terminate_before_start_errors() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            assert!(process.terminate_impl().is_err());
        });
    }

    #[test]
    fn pty_process_kill_before_start_errors() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            assert!(process.kill_impl().is_err());
        });
    }

    // ── Iteration 3: Utility function tests ──

    #[test]
    fn kill_process_tree_nonexistent_pid_is_noop() {
        kill_process_tree_impl(999999, 0.5);
    }

    #[test]
    fn get_process_tree_info_current_pid() {
        let pid = std::process::id();
        let info = native_get_process_tree_info(pid);
        assert!(info.contains(&format!("{}", pid)));
    }

    #[test]
    fn get_process_tree_info_nonexistent_pid() {
        let info = native_get_process_tree_info(999999);
        assert!(info.contains("Could not get process info"));
    }

    #[test]
    fn register_and_list_active_processes() {
        let fake_pid = 777777u32;
        register_active_process(
            fake_pid,
            "test",
            "echo hello",
            Some("/tmp".to_string()),
            1000.0,
        );
        let items = native_list_active_processes();
        assert!(items.iter().any(|e| e.0 == fake_pid));
        unregister_active_process(fake_pid);
        let items = native_list_active_processes();
        assert!(!items.iter().any(|e| e.0 == fake_pid));
    }

    #[test]
    fn process_created_at_current_process_returns_some() {
        let created = process_created_at(std::process::id());
        assert!(created.is_some());
        assert!(created.unwrap() > 0.0);
    }

    #[test]
    fn process_created_at_nonexistent_returns_none() {
        assert!(process_created_at(999999).is_none());
    }

    #[test]
    fn same_process_identity_current_process_matches() {
        let pid = std::process::id();
        let created = process_created_at(pid).unwrap();
        assert!(same_process_identity(pid, created, 2.0));
    }

    #[test]
    fn same_process_identity_wrong_time_no_match() {
        assert!(!same_process_identity(std::process::id(), 0.0, 1.0));
    }

    #[test]
    #[cfg(windows)]
    fn windows_apply_process_priority_current_pid_ok() {
        pyo3::prepare_freethreaded_python();
        assert!(windows_apply_process_priority_impl(std::process::id(), 0).is_ok());
    }

    #[test]
    #[cfg(windows)]
    fn windows_apply_process_priority_nonexistent_errors() {
        pyo3::prepare_freethreaded_python();
        assert!(windows_apply_process_priority_impl(999999, 0).is_err());
    }

    #[test]
    fn signal_bool_new_default_false() {
        assert!(!NativeSignalBool::new(false).load_nolock());
    }

    #[test]
    fn signal_bool_new_true() {
        assert!(NativeSignalBool::new(true).load_nolock());
    }

    #[test]
    fn signal_bool_store_locked_changes_value() {
        let sb = NativeSignalBool::new(false);
        sb.store_locked(true);
        assert!(sb.load_nolock());
    }

    #[test]
    fn signal_bool_compare_and_swap_success_iter3() {
        let sb = NativeSignalBool::new(false);
        assert!(sb.compare_and_swap_locked(false, true));
        assert!(sb.load_nolock());
    }

    #[test]
    fn idle_monitor_state_initial_values() {
        let state = IdleMonitorState {
            last_reset_at: Instant::now(),
            returncode: None,
            interrupted: false,
        };
        assert!(state.returncode.is_none());
        assert!(!state.interrupted);
    }

    #[test]
    #[cfg(windows)]
    fn terminal_input_wait_returns_event_immediately() {
        let state = Arc::new(Mutex::new(TerminalInputState {
            events: {
                let mut q = VecDeque::new();
                q.push_back(TerminalInputEventRecord {
                    data: b"x".to_vec(),
                    submit: false,
                    shift: false,
                    ctrl: false,
                    alt: false,
                    virtual_key_code: 0,
                    repeat_count: 1,
                });
                q
            },
            closed: false,
        }));
        let condvar = Arc::new(Condvar::new());
        match wait_for_terminal_input_event(&state, &condvar, Some(Duration::from_millis(100))) {
            TerminalInputWaitOutcome::Event(e) => assert_eq!(e.data, b"x"),
            _ => panic!("expected Event"),
        }
    }

    #[test]
    #[cfg(windows)]
    fn terminal_input_wait_returns_closed() {
        let state = Arc::new(Mutex::new(TerminalInputState {
            events: VecDeque::new(),
            closed: true,
        }));
        let condvar = Arc::new(Condvar::new());
        assert!(matches!(
            wait_for_terminal_input_event(&state, &condvar, Some(Duration::from_millis(100))),
            TerminalInputWaitOutcome::Closed
        ));
    }

    #[test]
    #[cfg(windows)]
    fn terminal_input_wait_returns_timeout() {
        let state = Arc::new(Mutex::new(TerminalInputState {
            events: VecDeque::new(),
            closed: false,
        }));
        let condvar = Arc::new(Condvar::new());
        assert!(matches!(
            wait_for_terminal_input_event(&state, &condvar, Some(Duration::from_millis(50))),
            TerminalInputWaitOutcome::Timeout
        ));
    }

    #[test]
    fn native_running_process_is_pty_available_false() {
        assert!(!NativeRunningProcess::is_pty_available());
    }

    #[test]
    #[cfg(not(windows))]
    fn posix_input_payload_passthrough() {
        // On POSIX, input_payload is a passthrough (data.to_vec())
        // This is now in running_process_core::pty::pty_posix
        let data = b"hello\n";
        assert_eq!(data.to_vec(), b"hello\n");
    }

    // ══════════════════════════════════════════════════════════════
    // Iteration 4: Windows PTY process lifecycle + NativeRunningProcess
    // ══════════════════════════════════════════════════════════════

    // ── Windows PTY process lifecycle tests ──
    //
    // Note: On Windows ConPTY, the child process cannot exit cleanly until
    // the master pipe is dropped. Therefore `wait_impl()` alone may block
    // indefinitely — use `close_impl()` (which drops handles then waits)
    // for lifecycle cleanup. Tests that need the exit code must use
    // `kill_impl()` which explicitly drops handles.

    #[test]
    #[cfg(windows)]
    fn pty_process_start_and_close_windows() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec![
                "python".to_string(),
                "-c".to_string(),
                "print('hello')".to_string(),
            ];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            assert!(process.pid().unwrap().is_some());
            // close drops handles then waits — this is the correct Windows lifecycle
            assert!(process.close_impl().is_ok());
        });
    }

    #[test]
    #[cfg(windows)]
    fn pty_process_poll_none_while_running_windows() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec![
                "python".to_string(),
                "-c".to_string(),
                "import time; time.sleep(5)".to_string(),
            ];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            assert!(
                core_pty::poll_pty_process(&process.handles, &process.returncode)
                    .unwrap()
                    .is_none()
            );
            let _ = process.close_impl();
        });
    }

    #[test]
    #[cfg(windows)]
    fn pty_process_kill_running_process_windows() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec![
                "python".to_string(),
                "-c".to_string(),
                "import time; time.sleep(0.1)".to_string(),
            ];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            assert!(process.kill_impl().is_ok());
        });
    }

    #[test]
    #[cfg(windows)]
    fn pty_process_terminate_running_process_windows() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec![
                "python".to_string(),
                "-c".to_string(),
                "import time; time.sleep(0.1)".to_string(),
            ];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            // On Windows, terminate delegates to kill
            assert!(process.terminate_impl().is_ok());
        });
    }

    #[test]
    #[cfg(windows)]
    fn pty_process_close_not_started_is_ok_windows() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            // close before start should be ok (handles are None)
            assert!(process.close_impl().is_ok());
        });
    }

    #[test]
    #[cfg(windows)]
    fn pty_process_start_already_started_errors_windows() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec![
                "python".to_string(),
                "-c".to_string(),
                "import time; time.sleep(0.1)".to_string(),
            ];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            let result = process.start_impl();
            assert!(result.is_err());
            let _ = process.close_impl();
        });
    }

    #[test]
    #[cfg(windows)]
    fn pty_process_resize_while_running_windows() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec![
                "python".to_string(),
                "-c".to_string(),
                "import time; time.sleep(2)".to_string(),
            ];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            assert!(process.resize_impl(40, 120).is_ok());
            let _ = process.close_impl();
        });
    }

    #[test]
    #[cfg(windows)]
    fn pty_process_write_windows() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec![
                "python".to_string(),
                "-c".to_string(),
                "import time; time.sleep(2)".to_string(),
            ];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            let _ = process.write_impl(b"hello\n", false);
            assert!(process.pty_input_bytes_total() >= 6);
            assert!(process.pty_newline_events_total() >= 1);
            let _ = process.close_impl();
        });
    }

    #[test]
    #[cfg(windows)]
    fn pty_process_input_metrics_tracked_windows() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec![
                "python".to_string(),
                "-c".to_string(),
                "import time; time.sleep(2)".to_string(),
            ];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            assert_eq!(process.pty_input_bytes_total(), 0);
            let _ = process.write_impl(b"hello\n", false);
            assert_eq!(process.pty_input_bytes_total(), 6);
            assert_eq!(process.pty_newline_events_total(), 1);
            let _ = process.write_impl(b"x", true);
            assert_eq!(process.pty_submit_events_total(), 1);
            let _ = process.close_impl();
        });
    }

    #[test]
    #[cfg(windows)]
    fn pty_process_send_interrupt_windows() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec![
                "python".to_string(),
                "-c".to_string(),
                "import time; time.sleep(0.1)".to_string(),
            ];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            // send_interrupt on Windows writes Ctrl+C byte via PTY
            assert!(process.send_interrupt_impl().is_ok());
            let _ = process.close_impl();
        });
    }

    #[test]
    #[cfg(windows)]
    fn pty_process_with_cwd_windows() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let tmp = std::env::temp_dir();
            let cwd = tmp.to_str().unwrap().to_string();
            let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
            let process = CoreNativePtyProcess::new(argv, Some(cwd), None, 24, 80, None).unwrap();
            process.start_impl().unwrap();
            assert!(process.close_impl().is_ok());
        });
    }

    #[test]
    #[cfg(windows)]
    fn pty_process_with_env_windows() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let mut env_pairs = Vec::new();
            if let Ok(path) = std::env::var("PATH") {
                env_pairs.push(("PATH".to_string(), path));
            }
            if let Ok(root) = std::env::var("SystemRoot") {
                env_pairs.push(("SystemRoot".to_string(), root));
            }
            env_pairs.push(("RP_TEST_PTY".to_string(), "test_value".to_string()));
            let argv = vec![
                "python".to_string(),
                "-c".to_string(),
                "import os; print(os.environ.get('RP_TEST_PTY', 'MISSING'))".to_string(),
            ];
            let process =
                CoreNativePtyProcess::new(argv, None, Some(env_pairs), 24, 80, None).unwrap();
            process.start_impl().unwrap();
            assert!(process.close_impl().is_ok());
        });
    }

    // ── Windows PTY terminal input relay tests ──

    #[test]
    #[cfg(windows)]
    fn pty_process_terminal_input_relay_not_active_initially() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            assert!(!process.terminal_input_relay_active.load(Ordering::Acquire));
        });
    }

    #[test]
    #[cfg(windows)]
    fn pty_process_stop_terminal_input_relay_noop_when_not_started() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            process.stop_terminal_input_relay_impl(); // should not panic
        });
    }

    // ── Windows-specific helper function tests ──

    #[test]
    #[cfg(windows)]
    fn assign_child_to_job_null_handle_errors() {
        pyo3::prepare_freethreaded_python();
        let result = assign_child_to_windows_kill_on_close_job(None);
        assert!(result.is_err());
    }

    #[test]
    #[cfg(windows)]
    fn apply_windows_pty_priority_none_handle_ok() {
        pyo3::prepare_freethreaded_python();
        // None handle with any nice value should be Ok (early return)
        assert!(apply_windows_pty_priority(None, Some(5)).is_ok());
        assert!(apply_windows_pty_priority(None, None).is_ok());
    }

    #[test]
    #[cfg(windows)]
    fn apply_windows_pty_priority_zero_nice_noop() {
        pyo3::prepare_freethreaded_python();
        // Some handle with nice=0 → flags=0 → early return Ok
        use std::os::windows::io::AsRawHandle;
        let current = std::process::Command::new("cmd")
            .args(["/C", "echo"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let handle = current.as_raw_handle();
        assert!(apply_windows_pty_priority(Some(handle), Some(0)).is_ok());
        assert!(apply_windows_pty_priority(Some(handle), None).is_ok());
    }

    // ── NativeRunningProcess lifecycle tests ──

    #[test]
    fn running_process_start_wait_lifecycle() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd = pyo3::types::PyList::new(py, ["python", "-c", "print('hello')"]).unwrap();
            let process = NativeRunningProcess::new(
                cmd.as_any(),
                None,
                false,
                true,
                None,
                None,
                true,
                None,
                None,
                "inherit",
                "stdout",
                None,
                false,
            )
            .unwrap();
            process.start_impl().unwrap();
            assert!(process.inner.pid().is_some());
            let code = process.wait_impl(py, Some(10.0)).unwrap();
            assert_eq!(code, 0);
        });
    }

    #[test]
    fn running_process_kill_running() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd =
                pyo3::types::PyList::new(py, ["python", "-c", "import time; time.sleep(0.1)"])
                    .unwrap();
            let process = NativeRunningProcess::new(
                cmd.as_any(),
                None,
                false,
                false,
                None,
                None,
                true,
                None,
                None,
                "inherit",
                "stdout",
                None,
                false,
            )
            .unwrap();
            process.start_impl().unwrap();
            assert!(process.kill_impl().is_ok());
        });
    }

    #[test]
    fn running_process_terminate_running() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd =
                pyo3::types::PyList::new(py, ["python", "-c", "import time; time.sleep(0.1)"])
                    .unwrap();
            let process = NativeRunningProcess::new(
                cmd.as_any(),
                None,
                false,
                false,
                None,
                None,
                true,
                None,
                None,
                "inherit",
                "stdout",
                None,
                false,
            )
            .unwrap();
            process.start_impl().unwrap();
            assert!(process.terminate_impl().is_ok());
        });
    }

    #[test]
    fn running_process_close_finished() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd = pyo3::types::PyList::new(py, ["python", "-c", "pass"]).unwrap();
            let process = NativeRunningProcess::new(
                cmd.as_any(),
                None,
                false,
                false,
                None,
                None,
                true,
                None,
                None,
                "inherit",
                "stdout",
                None,
                false,
            )
            .unwrap();
            process.start_impl().unwrap();
            let _ = process.wait_impl(py, Some(10.0));
            assert!(process.close_impl(py).is_ok());
        });
    }

    #[test]
    fn running_process_close_running() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd =
                pyo3::types::PyList::new(py, ["python", "-c", "import time; time.sleep(0.1)"])
                    .unwrap();
            let process = NativeRunningProcess::new(
                cmd.as_any(),
                None,
                false,
                false,
                None,
                None,
                true,
                None,
                None,
                "inherit",
                "stdout",
                None,
                false,
            )
            .unwrap();
            process.start_impl().unwrap();
            assert!(process.close_impl(py).is_ok());
        });
    }

    // ── NativeRunningProcess decode/text mode tests ──

    #[test]
    fn running_process_decode_line_text_mode() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd = pyo3::types::PyList::new(py, ["echo", "test"]).unwrap();
            let process = NativeRunningProcess::new(
                cmd.as_any(),
                None,
                false,
                true,
                None,
                None,
                true, // text=true
                None,
                None,
                "inherit",
                "stdout",
                None,
                false,
            )
            .unwrap();
            let result = process.decode_line_to_string(py, b"hello world").unwrap();
            assert_eq!(result, "hello world");
        });
    }

    #[test]
    fn running_process_decode_line_binary_mode() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd = pyo3::types::PyList::new(py, ["echo", "test"]).unwrap();
            let process = NativeRunningProcess::new(
                cmd.as_any(),
                None,
                false,
                true,
                None,
                None,
                false, // text=false
                None,
                None,
                "inherit",
                "stdout",
                None,
                false,
            )
            .unwrap();
            let result = process.decode_line_to_string(py, b"\xff\xfe").unwrap();
            // Binary mode uses lossy conversion
            assert!(!result.is_empty());
        });
    }

    #[test]
    fn running_process_decode_line_custom_encoding() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd = pyo3::types::PyList::new(py, ["echo", "test"]).unwrap();
            let process = NativeRunningProcess::new(
                cmd.as_any(),
                None,
                false,
                true,
                None,
                None,
                true,
                Some("ascii".to_string()),
                Some("replace".to_string()),
                "inherit",
                "stdout",
                None,
                false,
            )
            .unwrap();
            let result = process.decode_line_to_string(py, b"hello").unwrap();
            assert_eq!(result, "hello");
        });
    }

    #[test]
    fn running_process_captured_stream_text() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd =
                pyo3::types::PyList::new(py, ["python", "-c", "print('line1'); print('line2')"])
                    .unwrap();
            let process = NativeRunningProcess::new(
                cmd.as_any(),
                None,
                false,
                true,
                None,
                None,
                true,
                None,
                None,
                "inherit",
                "stdout",
                None,
                false,
            )
            .unwrap();
            process.start_impl().unwrap();
            let _ = process.wait_impl(py, Some(10.0));
            let text = process
                .captured_stream_text(py, StreamKind::Stdout)
                .unwrap();
            assert!(text.contains("line1"));
            assert!(text.contains("line2"));
        });
    }

    #[test]
    fn running_process_captured_combined_text() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd = pyo3::types::PyList::new(
                py,
                [
                    "python",
                    "-c",
                    "import sys; print('out'); print('err', file=sys.stderr)",
                ],
            )
            .unwrap();
            let process = NativeRunningProcess::new(
                cmd.as_any(),
                None,
                false,
                true,
                None,
                None,
                true,
                None,
                None,
                "inherit",
                "pipe",
                None,
                false,
            )
            .unwrap();
            process.start_impl().unwrap();
            let _ = process.wait_impl(py, Some(10.0));
            let text = process.captured_combined_text(py).unwrap();
            assert!(text.contains("out"));
            assert!(text.contains("err"));
        });
    }

    #[test]
    fn running_process_read_status_text_stream() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd = pyo3::types::PyList::new(py, ["python", "-c", "print('data')"]).unwrap();
            let process = NativeRunningProcess::new(
                cmd.as_any(),
                None,
                false,
                true,
                None,
                None,
                true,
                None,
                None,
                "inherit",
                "stdout",
                None,
                false,
            )
            .unwrap();
            process.start_impl().unwrap();
            let _ = process.wait_impl(py, Some(10.0));
            std::thread::sleep(Duration::from_millis(50));
            // Read from stdout
            let status = process
                .read_status_text(Some(StreamKind::Stdout), Some(Duration::from_millis(100)));
            assert!(status.is_ok());
        });
    }

    #[test]
    fn running_process_read_status_text_combined() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd = pyo3::types::PyList::new(py, ["python", "-c", "print('data')"]).unwrap();
            let process = NativeRunningProcess::new(
                cmd.as_any(),
                None,
                false,
                true,
                None,
                None,
                true,
                None,
                None,
                "inherit",
                "stdout",
                None,
                false,
            )
            .unwrap();
            process.start_impl().unwrap();
            let _ = process.wait_impl(py, Some(10.0));
            std::thread::sleep(Duration::from_millis(50));
            // Read from combined (None stream)
            let status = process.read_status_text(None, Some(Duration::from_millis(100)));
            assert!(status.is_ok());
        });
    }

    #[test]
    fn running_process_decode_line_returns_bytes_in_binary_mode() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd = pyo3::types::PyList::new(py, ["echo", "test"]).unwrap();
            let process = NativeRunningProcess::new(
                cmd.as_any(),
                None,
                false,
                true,
                None,
                None,
                false, // text=false → bytes mode
                None,
                None,
                "inherit",
                "stdout",
                None,
                false,
            )
            .unwrap();
            let result = process.decode_line(py, b"hello").unwrap();
            // In binary mode, should return PyBytes
            let bytes: Vec<u8> = result.extract(py).unwrap();
            assert_eq!(bytes, b"hello");
        });
    }

    #[test]
    fn running_process_decode_line_returns_string_in_text_mode() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd = pyo3::types::PyList::new(py, ["echo", "test"]).unwrap();
            let process = NativeRunningProcess::new(
                cmd.as_any(),
                None,
                false,
                true,
                None,
                None,
                true, // text=true → string mode
                None,
                None,
                "inherit",
                "stdout",
                None,
                false,
            )
            .unwrap();
            let result = process.decode_line(py, b"hello").unwrap();
            let text: String = result.extract(py).unwrap();
            assert_eq!(text, "hello");
        });
    }

    // ── NativePtyBuffer decode_chunk tests ──

    #[test]
    fn pty_buffer_decode_chunk_text_mode() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let buf = NativePtyBuffer::new(true, "utf-8", "replace");
            let result = buf.decode_chunk(py, b"hello").unwrap();
            let text: String = result.extract(py).unwrap();
            assert_eq!(text, "hello");
        });
    }

    #[test]
    fn pty_buffer_decode_chunk_binary_mode() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let buf = NativePtyBuffer::new(false, "utf-8", "replace");
            let result = buf.decode_chunk(py, b"\xff\xfe").unwrap();
            let bytes: Vec<u8> = result.extract(py).unwrap();
            assert_eq!(bytes, vec![0xff, 0xfe]);
        });
    }

    // ── NativePtyProcess mark_reader_closed / store_returncode tests ──

    #[test]
    fn pty_process_mark_reader_closed() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            // reader should not be closed initially
            assert!(!process.reader.state.lock().unwrap().closed);
            process.mark_reader_closed();
            assert!(process.reader.state.lock().unwrap().closed);
        });
    }

    #[test]
    fn pty_process_store_returncode_sets_value() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            assert!(process.returncode.lock().unwrap().is_none());
            process.store_returncode(42);
            assert_eq!(*process.returncode.lock().unwrap(), Some(42));
        });
    }

    #[test]
    fn pty_process_record_input_metrics_tracks_data() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|_py| {
            let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
            let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
            assert_eq!(process.pty_input_bytes_total(), 0);
            process.record_input_metrics(b"hello\n", false);
            assert_eq!(process.pty_input_bytes_total(), 6);
            assert_eq!(process.pty_newline_events_total(), 1);
            assert_eq!(process.pty_submit_events_total(), 0);
            process.record_input_metrics(b"\r", true);
            assert_eq!(process.pty_submit_events_total(), 1);
        });
    }

    // ── process_err_to_py additional variants ──

    #[test]
    fn process_err_to_py_already_started_is_runtime_error() {
        pyo3::prepare_freethreaded_python();
        let err = process_err_to_py(running_process_core::ProcessError::AlreadyStarted);
        pyo3::Python::with_gil(|py| {
            assert!(err.is_instance_of::<pyo3::exceptions::PyRuntimeError>(py));
        });
    }

    #[test]
    fn process_err_to_py_stdin_unavailable_is_runtime_error() {
        pyo3::prepare_freethreaded_python();
        let err = process_err_to_py(running_process_core::ProcessError::StdinUnavailable);
        pyo3::Python::with_gil(|py| {
            assert!(err.is_instance_of::<pyo3::exceptions::PyRuntimeError>(py));
        });
    }

    #[test]
    fn process_err_to_py_spawn_is_runtime_error() {
        pyo3::prepare_freethreaded_python();
        let err = process_err_to_py(running_process_core::ProcessError::Spawn(
            std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
        ));
        pyo3::Python::with_gil(|py| {
            assert!(err.is_instance_of::<pyo3::exceptions::PyRuntimeError>(py));
        });
    }

    #[test]
    fn process_err_to_py_io_is_runtime_error() {
        pyo3::prepare_freethreaded_python();
        let err = process_err_to_py(running_process_core::ProcessError::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "broken pipe",
        )));
        pyo3::Python::with_gil(|py| {
            assert!(err.is_instance_of::<pyo3::exceptions::PyRuntimeError>(py));
        });
    }

    // ── NativeRunningProcess: piped stdin tests ──

    #[test]
    fn running_process_piped_stdin() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd = pyo3::types::PyList::new(
                py,
                [
                    "python",
                    "-c",
                    "import sys; data=sys.stdin.buffer.read(); sys.stdout.buffer.write(data[::-1])",
                ],
            )
            .unwrap();
            let process = NativeRunningProcess::new(
                cmd.as_any(),
                None,
                false,
                true,
                None,
                None,
                true,
                None,
                None,
                "piped",
                "stdout",
                None,
                false,
            )
            .unwrap();
            process.start_impl().unwrap();
            process.inner.write_stdin(b"abc").unwrap();
            let code = process.wait_impl(py, Some(10.0)).unwrap();
            assert_eq!(code, 0);
        });
    }

    // ── NativeRunningProcess: shell mode ──

    #[test]
    fn running_process_shell_mode() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd = pyo3::types::PyString::new(py, "echo shell-mode-test");
            let process = NativeRunningProcess::new(
                cmd.as_any(),
                None,
                true, // shell=true
                true,
                None,
                None,
                true,
                None,
                None,
                "inherit",
                "stdout",
                None,
                false,
            )
            .unwrap();
            process.start_impl().unwrap();
            let code = process.wait_impl(py, Some(10.0)).unwrap();
            assert_eq!(code, 0);
        });
    }

    // ── NativeRunningProcess: send_interrupt before start errors ──

    #[test]
    fn running_process_send_interrupt_before_start_errors() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let cmd = pyo3::types::PyList::new(py, ["python", "-c", "pass"]).unwrap();
            let process = NativeRunningProcess::new(
                cmd.as_any(),
                None,
                false,
                false,
                None,
                None,
                true,
                None,
                None,
                "inherit",
                "stdout",
                None,
                false,
            )
            .unwrap();
            assert!(process.send_interrupt_impl().is_err());
        });
    }

    // ── NativeTerminalInput additional tests ──

    #[test]
    fn terminal_input_inject_multiple_events() {
        let input = NativeTerminalInput::new();
        {
            let mut state = input.inner.state.lock().unwrap();
            for i in 0..5 {
                state.events.push_back(TerminalInputEventRecord {
                    data: vec![b'a' + i],
                    submit: false,
                    shift: false,
                    ctrl: false,
                    alt: false,
                    virtual_key_code: 0,
                    repeat_count: 1,
                });
            }
        }
        assert!(input.available());
        let mut count = 0;
        while input.inner.next_event().is_some() {
            count += 1;
        }
        assert_eq!(count, 5);
        assert!(!input.available());
    }

    #[test]
    fn terminal_input_capturing_false_initially() {
        let input = NativeTerminalInput::new();
        assert!(!input.capturing());
    }

    // ── NativeTerminalInputEvent fields ──

    #[test]
    fn terminal_input_event_fields() {
        let event = NativeTerminalInputEvent {
            data: vec![0x1B, 0x5B, 0x41],
            submit: false,
            shift: true,
            ctrl: true,
            alt: false,
            virtual_key_code: 38,
            repeat_count: 2,
        };
        assert_eq!(event.data, vec![0x1B, 0x5B, 0x41]);
        assert!(!event.submit);
        assert!(event.shift);
        assert!(event.ctrl);
        assert!(!event.alt);
        assert_eq!(event.virtual_key_code, 38);
        assert_eq!(event.repeat_count, 2);
        // __repr__ should include all flags
        let repr = event.__repr__();
        assert!(repr.contains("shift=true"));
        assert!(repr.contains("ctrl=true"));
        assert!(repr.contains("alt=false"));
    }

    // ── spawn_pty_reader test ──

    #[test]
    fn spawn_pty_reader_reads_data_and_closes() {
        let shared = Arc::new(PtyReadShared {
            state: Mutex::new(PtyReadState {
                chunks: VecDeque::new(),
                closed: false,
            }),
            condvar: Condvar::new(),
        });

        let data = b"hello from reader\n";
        let reader: Box<dyn std::io::Read + Send> = Box::new(std::io::Cursor::new(data.to_vec()));
        let echo = Arc::new(AtomicBool::new(false));
        let idle = Arc::new(Mutex::new(None));
        let out_bytes = Arc::new(AtomicUsize::new(0));
        let churn_bytes = Arc::new(AtomicUsize::new(0));
        core_pty::spawn_pty_reader(
            reader,
            Arc::clone(&shared),
            echo,
            idle,
            out_bytes,
            churn_bytes,
        );

        // Wait for the reader thread to finish
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = shared.state.lock().unwrap();
            if state.closed {
                break;
            }
            drop(state);
            assert!(Instant::now() < deadline, "reader thread did not close");
            std::thread::sleep(Duration::from_millis(10));
        }

        let state = shared.state.lock().unwrap();
        assert!(state.closed);
        assert!(!state.chunks.is_empty());
    }

    #[test]
    fn spawn_pty_reader_empty_input_closes() {
        let shared = Arc::new(PtyReadShared {
            state: Mutex::new(PtyReadState {
                chunks: VecDeque::new(),
                closed: false,
            }),
            condvar: Condvar::new(),
        });

        let reader: Box<dyn std::io::Read + Send> = Box::new(std::io::Cursor::new(Vec::new()));
        let echo = Arc::new(AtomicBool::new(false));
        let idle = Arc::new(Mutex::new(None));
        let out_bytes = Arc::new(AtomicUsize::new(0));
        let churn_bytes = Arc::new(AtomicUsize::new(0));
        core_pty::spawn_pty_reader(
            reader,
            Arc::clone(&shared),
            echo,
            idle,
            out_bytes,
            churn_bytes,
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = shared.state.lock().unwrap();
            if state.closed {
                break;
            }
            drop(state);
            assert!(Instant::now() < deadline, "reader thread did not close");
            std::thread::sleep(Duration::from_millis(10));
        }

        let state = shared.state.lock().unwrap();
        assert!(state.closed);
        assert!(state.chunks.is_empty());
    }

    // ── Windows-only: windows_generate_console_ctrl_break ──

    #[test]
    #[cfg(windows)]
    fn windows_generate_console_ctrl_break_nonexistent_pid() {
        pyo3::prepare_freethreaded_python();
        // Nonexistent PID should error
        let result = windows_generate_console_ctrl_break_impl(999999, None);
        assert!(result.is_err());
    }

    // ── NativeRunningProcess: with env ──

    #[test]
    fn running_process_with_env() {
        pyo3::prepare_freethreaded_python();
        pyo3::Python::with_gil(|py| {
            let env = pyo3::types::PyDict::new(py);
            if let Ok(path) = std::env::var("PATH") {
                env.set_item("PATH", &path).unwrap();
            }
            #[cfg(windows)]
            if let Ok(root) = std::env::var("SystemRoot") {
                env.set_item("SystemRoot", &root).unwrap();
            }
            env.set_item("RP_TEST_VAR", "test_value").unwrap();

            let cmd = pyo3::types::PyList::new(
                py,
                [
                    "python",
                    "-c",
                    "import os; print(os.environ.get('RP_TEST_VAR', 'MISSING'))",
                ],
            )
            .unwrap();
            let process = NativeRunningProcess::new(
                cmd.as_any(),
                None,
                false,
                true,
                Some(env),
                None,
                true,
                None,
                None,
                "inherit",
                "stdout",
                None,
                false,
            )
            .unwrap();
            process.start_impl().unwrap();
            let code = process.wait_impl(py, Some(10.0)).unwrap();
            assert_eq!(code, 0);
            let text = process
                .captured_stream_text(py, StreamKind::Stdout)
                .unwrap();
            assert!(text.contains("test_value"));
        });
    }

    // ── Windows input_payload test ──

    #[test]
    #[cfg(windows)]
    fn windows_pty_input_payload_via_module() {
        assert_eq!(core_pty::windows_terminal_input_payload(b"hello"), b"hello");
        assert_eq!(core_pty::windows_terminal_input_payload(b"\n"), b"\r");
    }
}

// ── ContainedProcessGroup Python wrapper ────────────────────────────────────

/// Python enum-like class for containment policy.
#[pyclass]
#[derive(Clone, Copy)]
struct PyContainment {
    inner: Containment,
}

#[pymethods]
impl PyContainment {
    /// Create a "Contained" policy — child is killed when the group drops.
    #[staticmethod]
    fn contained() -> Self {
        Self {
            inner: Containment::Contained,
        }
    }

    /// Create a "Detached" policy — child survives the group drop.
    #[staticmethod]
    fn detached() -> Self {
        Self {
            inner: Containment::Detached,
        }
    }

    fn __repr__(&self) -> String {
        match self.inner {
            Containment::Contained => "Containment.Contained".to_string(),
            Containment::Detached => "Containment.Detached".to_string(),
        }
    }
}

/// Python wrapper for `ContainedProcessGroup`.
#[pyclass(name = "ContainedProcessGroup")]
struct PyContainedProcessGroup {
    inner: Option<ContainedProcessGroup>,
    children: Vec<ContainedChild>,
}

#[pymethods]
impl PyContainedProcessGroup {
    #[new]
    #[pyo3(signature = (originator=None))]
    fn new(originator: Option<String>) -> PyResult<Self> {
        let group = match originator {
            Some(ref orig) => ContainedProcessGroup::with_originator(orig).map_err(to_py_err)?,
            None => ContainedProcessGroup::new().map_err(to_py_err)?,
        };
        Ok(Self {
            inner: Some(group),
            children: Vec::new(),
        })
    }

    #[getter]
    fn originator(&self) -> Option<String> {
        self.inner.as_ref()?.originator().map(String::from)
    }

    #[getter]
    fn originator_value(&self) -> Option<String> {
        self.inner.as_ref()?.originator_value()
    }

    /// Spawn a contained child process (killed when group drops).
    fn spawn(&mut self, argv: Vec<String>) -> PyResult<u32> {
        let group = self
            .inner
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("group already closed"))?;
        if argv.is_empty() {
            return Err(PyValueError::new_err("argv must not be empty"));
        }
        let mut cmd = std::process::Command::new(&argv[0]);
        if argv.len() > 1 {
            cmd.args(&argv[1..]);
        }
        let contained = group.spawn(&mut cmd).map_err(to_py_err)?;
        let pid = contained.child.id();
        self.children.push(contained);
        Ok(pid)
    }

    /// Spawn a detached child process (survives group drop).
    fn spawn_detached(&mut self, argv: Vec<String>) -> PyResult<u32> {
        let group = self
            .inner
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("group already closed"))?;
        if argv.is_empty() {
            return Err(PyValueError::new_err("argv must not be empty"));
        }
        let mut cmd = std::process::Command::new(&argv[0]);
        if argv.len() > 1 {
            cmd.args(&argv[1..]);
        }
        let contained = group.spawn_detached(&mut cmd).map_err(to_py_err)?;
        let pid = contained.child.id();
        self.children.push(contained);
        Ok(pid)
    }

    /// Close the group, killing all contained children.
    fn close(&mut self) {
        self.inner.take();
    }

    /// Context manager: __enter__ returns self.
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Context manager: __exit__ closes the group.
    #[pyo3(signature = (_exc_type=None, _exc_val=None, _exc_tb=None))]
    fn __exit__(
        &mut self,
        _exc_type: Option<&Bound<'_, PyAny>>,
        _exc_val: Option<&Bound<'_, PyAny>>,
        _exc_tb: Option<&Bound<'_, PyAny>>,
    ) {
        self.close();
    }
}

// ── Originator process scanning ─────────────────────────────────────────────

#[pyclass(name = "OriginatorProcessInfo")]
#[derive(Clone)]
struct PyOriginatorProcessInfo {
    #[pyo3(get)]
    pid: u32,
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    command: String,
    #[pyo3(get)]
    originator: String,
    #[pyo3(get)]
    parent_pid: u32,
    #[pyo3(get)]
    parent_alive: bool,
}

#[pymethods]
impl PyOriginatorProcessInfo {
    fn __repr__(&self) -> String {
        format!(
            "OriginatorProcessInfo(pid={}, name={:?}, originator={:?}, parent_pid={}, parent_alive={})",
            self.pid, self.name, self.originator, self.parent_pid, self.parent_alive
        )
    }
}

impl From<OriginatorProcessInfo> for PyOriginatorProcessInfo {
    fn from(info: OriginatorProcessInfo) -> Self {
        Self {
            pid: info.pid,
            name: info.name,
            command: info.command,
            originator: info.originator,
            parent_pid: info.parent_pid,
            parent_alive: info.parent_alive,
        }
    }
}

/// Find all processes whose RUNNING_PROCESS_ORIGINATOR env var starts
/// with the given tool prefix.
#[pyfunction]
fn py_find_processes_by_originator(tool: &str) -> Vec<PyOriginatorProcessInfo> {
    find_processes_by_originator(tool)
        .into_iter()
        .map(PyOriginatorProcessInfo::from)
        .collect()
}

/// Monitor for new visible windows that appear within the given duration.
///
/// Returns a list of dicts, each with keys: ``pid`` (int), ``title`` (str),
/// ``hwnd`` (int).  On non-Windows platforms this always returns an empty list.
#[pyfunction]
fn monitor_console_windows(py: Python<'_>, duration_secs: f64) -> PyResult<PyObject> {
    let duration = Duration::from_secs_f64(duration_secs);
    let infos = running_process_core::monitor_console_windows(duration);
    let list = PyList::empty(py);
    for info in infos {
        let dict = PyDict::new(py);
        dict.set_item("pid", info.pid)?;
        dict.set_item("title", &info.title)?;
        dict.set_item("hwnd", info.hwnd)?;
        list.append(dict)?;
    }
    Ok(list.into())
}

#[pymodule]
fn _native(_py: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyNativeProcess>()?;
    module.add_class::<NativeRunningProcess>()?;
    module.add_class::<PyContainedProcessGroup>()?;
    module.add_class::<PyContainment>()?;
    module.add_class::<PyOriginatorProcessInfo>()?;
    module.add_function(wrap_pyfunction!(py_find_processes_by_originator, module)?)?;
    module.add_class::<NativePtyProcess>()?;
    module.add_class::<NativeProcessMetrics>()?;
    module.add_class::<NativeSignalBool>()?;
    module.add_class::<NativeIdleDetector>()?;
    module.add_class::<NativePtyBuffer>()?;
    module.add_class::<NativeTerminalInput>()?;
    module.add_class::<NativeTerminalInputEvent>()?;
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
    module.add_function(wrap_pyfunction!(monitor_console_windows, module)?)?;
    module.add("VERSION", PyString::new(_py, env!("CARGO_PKG_VERSION")))?;
    Ok(())
}
