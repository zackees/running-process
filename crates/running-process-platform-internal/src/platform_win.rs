//! Windows implementation root for the process capability.

#[path = "platform_win/console.rs"]
mod console;
pub use console::monitor_console_windows;

#[path = "platform_win_descendants.rs"]
mod descendants;
pub use descendants::{assign_child_to_windows_job, WindowsJobHandle};

pub fn exact_trace_capability() -> crate::platform::process::ExactTraceCapability {
    crate::platform::process::ExactTraceCapability {
        available: false,
        backend: "windows-debug-process",
        reason: "the exact DEBUG_PROCESS supervisor is not available in this build",
        non_invasive_backend: "job-object-iocp",
        non_invasive_grade:
            crate::platform::process::NonInvasiveObservationGrade::KernelNotification,
    }
}

pub fn current_executable_build_id() -> Option<Vec<u8>> {
    None
}

pub struct TracedChild(std::process::Child);

impl TracedChild {
    pub fn id(&self) -> u32 {
        self.0.id()
    }

    pub fn try_wait_code(&mut self) -> std::io::Result<Option<i32>> {
        self.0.try_wait().map(|status| status.map(exit_code))
    }

    pub fn kill(&mut self) -> std::io::Result<()> {
        self.0.kill()
    }

    pub fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.0.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.0.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.0.stderr.take()
    }

    pub fn wait_code(&mut self) -> std::io::Result<i32> {
        self.0.wait().map(exit_code)
    }
}

pub fn configure_exact_trace(_command: &mut std::process::Command) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        exact_trace_capability().reason,
    ))
}

pub fn start_exact_trace(
    _command: std::process::Command,
    _emit: Box<dyn Fn(crate::platform::process::ExactTraceEvent) + Send>,
    _complete: Box<dyn FnOnce() + Send>,
) -> std::io::Result<TracedChild> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        exact_trace_capability().reason,
    ))
}

pub fn shell_command(command: &str) -> std::process::Command {
    let mut shell = std::process::Command::new("cmd.exe");
    shell.args(["/D", "/S", "/C", command]);
    shell
}

pub fn compat_shell_command(command: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;

    let mut shell = std::process::Command::new("cmd");
    shell.raw_arg("/D /S /C \"");
    shell.raw_arg(command);
    shell.raw_arg("\"");
    shell
}

pub fn canonical_environment_pairs(pairs: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut seen = std::collections::BTreeMap::new();
    for (key, value) in pairs {
        seen.insert(key.to_ascii_uppercase(), (key, value));
    }
    seen.into_values().collect()
}

/// Windows reports descendants through its Job Object IOCP path during spawn.
pub fn start_descendant_monitor(
    _root_pid: u32,
    _stop: std::sync::Arc<crate::platform::process::DescendantMonitorStop>,
    _emit: Box<dyn Fn(crate::platform::process::DescendantEvent) + Send>,
) -> std::io::Result<()> {
    Ok(())
}

use std::ffi::OsStr;
use std::io;
use std::io::Read;
use std::os::windows::io::AsRawHandle;
use std::sync::Mutex;
use std::sync::OnceLock;

use tokio::process::{Child, Command};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER};
use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

use crate::SpawnSpec;

#[derive(Default)]
pub struct CaptureCancellation { handles: Mutex<CaptureHandles> }
#[derive(Default)]
struct CaptureHandles { stdout: Option<usize>, stderr: Option<usize> }
pub fn prepare_capture_reader<R>(reader: R, cancellation: &CaptureCancellation, stream: crate::platform::process::CaptureStream) -> io::Result<Box<dyn Read + Send>>
where R: Read + AsRawHandle + Send + 'static {
    let mut handles = cancellation.handles.lock().expect("capture pipe handles mutex poisoned");
    match stream { crate::platform::process::CaptureStream::Stdout => handles.stdout = Some(reader.as_raw_handle() as usize), crate::platform::process::CaptureStream::Stderr => handles.stderr = Some(reader.as_raw_handle() as usize) }
    Ok(Box::new(reader))
}
pub fn capture_reader_done(cancellation: &CaptureCancellation, stream: crate::platform::process::CaptureStream) {
    let mut handles = cancellation.handles.lock().expect("capture pipe handles mutex poisoned");
    match stream { crate::platform::process::CaptureStream::Stdout => handles.stdout = None, crate::platform::process::CaptureStream::Stderr => handles.stderr = None }
}
pub fn cancel_capture_reader(cancellation: &CaptureCancellation) {
    use winapi::shared::ntdef::HANDLE;
    use winapi::um::ioapiset::CancelIoEx;
    let handles = cancellation.handles.lock().expect("capture pipe handles mutex poisoned");
    for handle in [handles.stdout, handles.stderr].into_iter().flatten() {
        // SAFETY: the slot remains populated until its reader completion callback runs.
        unsafe { CancelIoEx(handle as HANDLE, std::ptr::null_mut()); }
    }
}

#[path = "platform_win_file_handles.rs"]
mod file_handles;
pub use file_handles::read_process_file_handles;
#[path = "platform_win_cmdline.rs"]
mod cmdline;
pub use cmdline::read_process_cmdline;

#[path = "platform/process_tree.rs"]
mod process_tree;

pub fn kill_tree(pid: u32, timeout: std::time::Duration) -> io::Result<u32> {
    process_tree::kill_tree(pid, timeout, process_start_key)
}

pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

pub fn set_process_name(_name: &str) {}

pub fn configure_trampoline_command(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
}

pub fn configure_process_command(
    command: &mut std::process::Command,
    config: crate::platform::process::ProcessCommandConfig,
) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    let caller = config.creation_flags.unwrap_or(0);
    let group = if config.create_process_group { CREATE_NEW_PROCESS_GROUP } else { 0 };
    let caller_has_console_opinion =
        caller & (CREATE_NO_WINDOW | CREATE_NEW_CONSOLE | DETACHED_PROCESS) != 0;
    let no_window = if caller_has_console_opinion || parent_has_console() { 0 } else { CREATE_NO_WINDOW };
    let priority = match config.nice {
        Some(value) if value >= 15 => 0x0000_0040,
        Some(value) if value >= 1 => 0x0000_4000,
        Some(value) if value <= -15 => 0x0000_0080,
        Some(value) if value <= -1 => 0x0000_8000,
        _ => 0,
    };
    let flags = caller | group | no_window | priority;
    if flags != 0 {
        command.creation_flags(flags);
    }
    Ok(())
}

pub fn trampoline_exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

/// Request a Ctrl+Break event for a child-owned Windows process group.
pub fn soft_terminate_process_group(pid: u32) -> io::Result<()> {
    // SAFETY: the Windows API receives only a numeric process-group id and
    // does not retain Rust pointers or references.
    let ok = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
    if ok == 0 {
        let error = io::Error::last_os_error();
        // A closed or detached console target no longer needs a soft step.
        if error.raw_os_error() != Some(ERROR_INVALID_HANDLE as i32) {
            return Err(error);
        }
    }
    Ok(())
}

pub fn process_snapshot() -> Vec<crate::platform::process::ProcessSnapshot> {
    Vec::new()
}

pub fn process_snapshot_for_pid(_pid: u32) -> Option<crate::platform::process::ProcessSnapshot> {
    None
}

/// Windows compatibility stub for the Unix post-fork descriptor hook.
///
/// # Safety
/// This shares the Unix API contract and may only be called from the spawn
/// layer's post-fork/pre-exec boundary, even though it is a no-op on Windows.
pub unsafe fn unix_mark_extra_fds_close_on_exec() {}

pub fn configure_sync_daemon_command(_command: &mut std::process::Command) -> io::Result<()> { Ok(()) }

pub fn configure_sync_contained_command(_command: &mut std::process::Command) -> io::Result<()> { Ok(()) }

pub fn parent_has_console() -> bool {
    unsafe { windows_sys::Win32::System::Console::GetConsoleCP() != 0 }
}

pub fn sync_child_native_handle(child: &std::process::Child) -> usize {
    use std::os::windows::io::AsRawHandle;
    child.as_raw_handle() as usize
}
pub fn observer_backend(scope: crate::platform::process::ObserverScope, category: crate::platform::process::ObserverCategory) -> crate::platform::process::ObserverBackend {
    use crate::platform::process::{ObserverBackend as B, ObserverCategory as C, ObserverScope as S, ObserverSupport as P};
    match (scope, category) {
        (S::SystemWide, C::File) | (S::SystemWide, C::Network) | (S::SystemWide, C::Process) => B { support:P::Unavailable, backend:"etw", reason:"Phase 3: Windows ETW backend not yet implemented" },
        (S::LaunchedProcessTree, C::File) => B { support:P::Partial, backend:"nt-handle-snapshot", reason:"Windows NtQuerySystemInformation + DuplicateHandle + NtQueryObject snapshot via read_process_file_handles (#539 slice 4; no streaming file events)" },
        (S::LaunchedProcessTree, C::Network) => B { support:P::Unavailable, backend:"none", reason:"#539: no-admin per-child network backend deferred to a follow-up issue" },
        (S::LaunchedProcessTree, C::Process) => B { support:P::Supported, backend:"job-object-iocp", reason:"Windows Job Object IOCP descendant lifecycle (#539 slice 2)" },
    }
}

pub fn unix_set_priority(_pid: u32, _nice: i32) -> io::Result<()> { Err(io::Error::new(io::ErrorKind::Unsupported, "Unix priority is unavailable on Windows")) }
pub fn unix_signal_process(_pid: u32, _signal: crate::platform::process::UnixSignalKind) -> io::Result<()> { Err(io::Error::new(io::ErrorKind::Unsupported, "Unix signals are unavailable on Windows")) }
pub fn unix_signal_process_group(_pid: i32, _signal: crate::platform::process::UnixSignalKind) -> io::Result<()> { Err(io::Error::new(io::ErrorKind::Unsupported, "Unix signals are unavailable on Windows")) }
pub fn unix_signal_raw(_signal: crate::platform::process::UnixSignalKind) -> i32 { 0 }

pub fn configure_compat_tokio_command(
    command: &mut Command,
    show_console: bool,
    _kill_when_owner_dies: bool,
) -> io::Result<()> {
    let flags = compat_tokio_creation_flags(show_console);
    if flags != 0 {
        command.creation_flags(flags);
    }
    Ok(())
}

fn compat_tokio_creation_flags(show_console: bool) -> u32 {
    if show_console {
        0
    } else {
        0x0800_0000 // CREATE_NO_WINDOW
    }
}

pub fn after_compat_tokio_spawn(child: &Child, kill_when_owner_dies: bool) {
    if kill_when_owner_dies {
        assign(child.raw_handle());
    }
}

fn process_start_key(pid: sysinfo::Pid, _process: &sysinfo::Process) -> io::Result<u64> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid.as_u32()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    let queried = unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    let query_error = if queried == 0 { Some(io::Error::last_os_error()) } else { None };
    unsafe { CloseHandle(handle); }
    if let Some(error) = query_error {
        return Err(error);
    }
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

pub(crate) fn configure_command(
    command: &mut Command,
    create_process_group: bool,
    _kill_when_owner_dies: bool,
) -> io::Result<()> {
    if create_process_group {
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    Ok(())
}

pub(crate) fn after_spawn(child: &Child, kill_when_owner_dies: bool) {
    if kill_when_owner_dies {
        assign(child.raw_handle());
    }
}

pub(crate) fn signal_process(pid: u32) -> io::Result<()> {
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) { Ok(()) } else { Err(error) };
    }
    let terminated = unsafe { TerminateProcess(handle, 1) };
    let termination_error = if terminated == 0 { Some(io::Error::last_os_error()) } else { None };
    unsafe { CloseHandle(handle) };
    termination_error.map_or(Ok(()), Err)
}

pub(crate) fn signal_process_group(pid: u32) -> io::Result<()> {
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) } != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_INVALID_HANDLE as i32) { Ok(()) } else { Err(error) }
}

pub(crate) fn shell_spec(command: &OsStr) -> SpawnSpec {
    SpawnSpec::new("cmd.exe").arg("/C").arg(command)
}

struct Job(HANDLE);
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

static JOB: OnceLock<Option<Job>> = OnceLock::new();

fn create() -> Option<Job> {
    unsafe {
        let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if handle.is_null() { return None; }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(handle, JobObjectExtendedLimitInformation, &info as *const _ as *const _, std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32) == 0 { return None; }
        Some(Job(handle))
    }
}

fn assign(child: Option<HANDLE>) {
    let Some(child) = child else { return };
    let Some(job) = JOB.get_or_init(create).as_ref() else { return };
    unsafe { AssignProcessToJobObject(job.0, child); }
}
#[path = "platform_win/sync_spawn.rs"]
mod sync_spawn;
pub use sync_spawn::{spawn_sync, spawn_sync_daemon};

#[cfg(test)]
mod tests {
    use super::compat_tokio_creation_flags;
    use std::ffi::OsStr;

    #[test]
    fn tokio_spawn_owns_console_creation_flags() {
        assert_eq!(compat_tokio_creation_flags(false), 0x0800_0000);
        assert_eq!(compat_tokio_creation_flags(true), 0);
    }

    #[test]
    fn shell_command_preserves_round_trippable_cmd_quoting_contract() {
        let command_text = "echo alpha beta ^& gamma";
        let mut command = super::shell_command(command_text);
        assert_eq!(command.get_program(), OsStr::new("cmd.exe"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                OsStr::new("/D"),
                OsStr::new("/S"),
                OsStr::new("/C"),
                OsStr::new(command_text)
            ]
        );
        let output = command.output().expect("shell command should execute");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"alpha beta & gamma\r\n");
    }

    #[test]
    fn compat_shell_command_preserves_nested_quotes_for_standard_spawn() {
        let command_text = "if \"alpha beta\"==\"alpha beta\" (echo shell-ok)";
        let mut command = super::compat_shell_command(command_text);
        assert_eq!(command.get_program(), OsStr::new("cmd"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                OsStr::new("/D /S /C \""),
                OsStr::new(command_text),
                OsStr::new("\"")
            ]
        );
        let output = command.output().expect("compat shell command should execute");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"shell-ok\r\n");
    }
}
