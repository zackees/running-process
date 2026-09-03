//! Windows implementation root for the process capability.

#[path = "platform_win/autostart.rs"]
pub(crate) mod autostart;

#[path = "platform_win/resources.rs"]
pub(crate) mod resources;
pub use resources::{
    fd_exhaustion_error as resources_fd_exhaustion_error,
    inode_capacity as resources_inode_capacity,
    signals_fd_exhaustion as resources_signals_fd_exhaustion,
    signals_storage_exhaustion as resources_signals_storage_exhaustion,
    storage_exhaustion_error as resources_storage_exhaustion_error,
};

pub use autostart::{
    register as autostart_register,
    render_registration as autostart_render_registration,
    unregister as autostart_unregister,
};

#[path = "platform_win/process_inspect.rs"]
pub(crate) mod process_inspect;
pub use process_inspect::{
    process_executable_path, process_force_kill, process_same_executable_path,
    process_signal_terminate, ProcessLiveness,
};

#[path = "platform_win/raw_write.rs"]
pub(crate) mod raw_write;
pub use raw_write::write_all_to_descriptor as fs_write_all_to_descriptor;

#[path = "platform_win/shutdown_request.rs"]
pub(crate) mod shutdown_request;
pub use shutdown_request::install_shutdown_request_handler as process_install_shutdown_request_handler;

#[path = "platform_win/process_owner_death.rs"]
pub(crate) mod process_owner_death;
pub use process_owner_death::{
    install_owner_death_cleanup as process_install_owner_death_cleanup,
    owner_death_cleanup_target as process_owner_death_cleanup_target,
};

#[path = "platform_win/host.rs"]
pub(crate) mod host;
pub use host::{
    boot_id as host_boot_id, current_process_privilege as host_current_process_privilege,
    environment_keys_are_case_insensitive as host_environment_keys_are_case_insensitive,
    filesystem_device_id as host_filesystem_device_id, hostname as host_hostname,
    login_environment as host_login_environment, machine_id as host_machine_id,
    namespace_id as host_namespace_id, user_machine_identity as host_user_machine_identity,
    PrivilegedIdentity as HostPrivilegedIdentity,
};
pub use host::login_environment_block as host_login_environment_block;

#[cfg(feature = "fs")]
#[path = "platform_win/fs.rs"]
pub(crate) mod fs;
#[cfg(feature = "fs")]
pub use fs::{
    create_private_file as fs_create_private_file,
    decode_path_bytes as fs_decode_path_bytes,
    replace_file as fs_replace_file, sync_directory as fs_sync_directory,
    user_config_dir as fs_user_config_dir,
    user_data_dir as fs_user_data_dir, encode_path_bytes as fs_encode_path_bytes,
    file_identity as fs_file_identity, is_lock_conflict as fs_is_lock_conflict,
    open_lock_file as fs_open_lock_file, path_identity as fs_path_identity,
    try_lock_exclusive as fs_try_lock_exclusive, unlock as fs_unlock,
    user_run_data_root as fs_user_run_data_root, user_runtime_dir as fs_user_runtime_dir,
    user_state_dir as fs_user_state_dir, FileIdentity as FsFileIdentity,
};

#[path = "platform_win/executable.rs"]
pub(crate) mod executable;
pub use executable::{
    file_name as executable_file_name,
    sibling_of_current_image as executable_sibling_of_current_image,
    EXECUTABLE_EXTENSION,
};

#[cfg(feature = "ipc")]
#[path = "platform_win/ipc.rs"]
pub(crate) mod ipc;
#[cfg(feature = "private-dir")]
#[path = "platform_win/ipc_private_dir.rs"]
mod ipc_private_dir;
#[cfg(feature = "ipc")]
pub use ipc::{
    current_user_id as ipc_current_user_id, Endpoint as IpcEndpoint,
    endpoint_is_filesystem_backed as ipc_endpoint_is_filesystem_backed,
    nonblocking_zero_read_is_pending as ipc_nonblocking_zero_read_is_pending,
    select_endpoint_address as ipc_select_endpoint_address,
    InheritedListener as IpcInheritedListener, Listener as IpcListener,
    ListenerNonblockingMode as IpcListenerNonblockingMode, PeerIdentity as IpcPeerIdentity,
    PeerIdentitySource as IpcPeerIdentitySource, Stream as IpcStream,
};
#[cfg(feature = "ipc")]
pub const LEGACY_SCM_RIGHTS_TRANSPORT_SUPPORTED: bool = false;
#[cfg(feature = "ipc")]
pub const LEGACY_DUPLICATE_HANDLE_TRANSPORT_SUPPORTED: bool = true;
#[cfg(feature = "ipc")]
pub use ipc::legacy_duplicate_handle;
#[cfg(feature = "ipc")]
pub fn legacy_send_fd_to(
    _socket: &std::path::Path,
    _sent_fd: i32,
    _payload: &[u8],
) -> Result<(), crate::LegacyHandoffError> {
    Err(crate::LegacyHandoffError::new(
        crate::platform::ipc::HandoffTransferErrorKind::Unsupported,
        None,
    ))
}
#[cfg(feature = "ipc")]
pub fn legacy_send_fd_over(
    _socket_fd: i32,
    _sent_fd: i32,
    _payload: &[u8],
) -> Result<(), crate::LegacyHandoffError> {
    Err(crate::LegacyHandoffError::new(
        crate::platform::ipc::HandoffTransferErrorKind::Unsupported,
        None,
    ))
}
#[cfg(feature = "private-dir")]
pub use ipc_private_dir::{
    ensure_owner_private_directory as private_dir_ensure_owner_private_directory,
    owner_private_directory as private_dir_owner_private_directory,
};
#[cfg(feature = "ipc")]
pub fn ipc_broker_endpoint_name(bare_name: &str, _path_scoped: bool) -> std::io::Result<String> {
    Ok(format!(r"\\.\pipe\{bare_name}"))
}

/// Windows named-pipe names are capped by `MAX_PATH` while the long-path
/// prefix is not in use.
#[cfg(feature = "ipc")]
const WINDOWS_MAX_PATH: usize = 260;

#[cfg(feature = "ipc")]
pub fn ipc_endpoint_name_limit() -> crate::platform::ipc::EndpointNameLimit {
    crate::platform::ipc::EndpointNameLimit {
        max_bytes: WINDOWS_MAX_PATH,
        label: "Windows MAX_PATH",
    }
}

#[cfg(feature = "ipc")]
pub fn ipc_broker_v1_endpoint_path(
    bare_name: &str,
) -> Result<String, crate::platform::ipc::EndpointNameTooLong> {
    let path = format!(r"\\.\pipe\{bare_name}");
    if path.len() > WINDOWS_MAX_PATH {
        return Err(crate::platform::ipc::EndpointNameTooLong {
            len: path.len(),
            max: WINDOWS_MAX_PATH,
            limit_label: "Windows MAX_PATH",
        });
    }
    Ok(path)
}

#[cfg(feature = "ipc")]
pub fn ipc_endpoint_scope_bytes(path: &std::path::Path) -> Vec<u8> {
    // Windows paths and named pipes are case-insensitive. Hash one
    // slash/case-normalized spelling so callers cannot split the broker
    // merely by varying path presentation.
    path.to_string_lossy()
        .replace('\\', "/")
        .to_lowercase()
        .into_bytes()
}

#[cfg(feature = "ipc")]
pub fn ipc_broker_v2_runtime_dir() -> std::path::PathBuf {
    // Named pipes have no directory, so this is chosen rather than derived.
    // `data_local_dir` is per-user and non-roaming, which is what a
    // machine-local endpoint file wants -- a roaming profile would carry a
    // port from another machine.
    dirs::data_local_dir()
        .map(|dir| dir.join("running-process").join("broker-v2"))
        .unwrap_or_else(crate::platform::ipc::per_user_runtime_fallback)
}
#[cfg(feature = "ipc")]
pub fn into_legacy_ipc_stream(stream: IpcStream) -> interprocess::local_socket::Stream {
    stream.0
}

#[cfg(feature = "ipc")]
pub fn from_legacy_ipc_stream(stream: interprocess::local_socket::Stream) -> IpcStream {
    ipc::Stream(stream)
}
#[cfg(feature = "ipc")]
pub fn legacy_ipc_name(path: &str) -> Result<interprocess::local_socket::Name<'_>, String> {
    ipc::legacy_name(path)
}
#[cfg(feature = "ipc-async")]
pub use ipc::{
    AsyncListener as IpcAsyncListener, AsyncStream as IpcAsyncStream,
    IntoAsyncListener as IpcIntoAsyncListener, IntoAsyncStream as IpcIntoAsyncStream,
};

#[cfg(feature = "session-relay")]
#[path = "platform_win_session_relay.rs"]
mod session_relay;
#[cfg(feature = "session-relay")]
pub use session_relay::relay_local_socket_session;

#[path = "platform_win/console.rs"]
mod console;
pub use console::monitor_console_windows;

#[cfg(feature = "pty")]
#[path = "platform_win/terminal.rs"]
pub mod terminal;
#[cfg(feature = "terminal-graphics")]
#[path = "platform_win/terminal_graphics.rs"]
mod terminal_graphics;
#[cfg(feature = "terminal-graphics")]
pub use terminal_graphics::active_graphics_probe;

#[path = "platform_win/terminal_input.rs"]
pub mod terminal_input;

#[path = "platform_win/window_icon.rs"]
mod window_icon;
pub use window_icon::{icon_support as window_icon_support_impl, set_icon as set_window_icon_impl};

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

#[cfg(feature = "async-process")]
use std::ffi::OsStr;
use std::io;
use std::io::Read;
use std::os::windows::io::AsRawHandle;
use std::sync::Mutex;
#[cfg(feature = "async-process")]
use std::sync::OnceLock;

#[cfg(feature = "async-process")]
use tokio::process::{Child, Command};
// Each import carries the gate its users carry, so a build with neither
// `async-process` nor `process-inspection` does not import symbols nothing
// references. `async-process` implies `process-inspection`, so the latter is
// the wider gate of the two. `ERROR_INVALID_HANDLE` stays ungated:
// `soft_terminate_process_group` uses it unconditionally.
use windows_sys::Win32::Foundation::ERROR_INVALID_HANDLE;
#[cfg(feature = "process-inspection")]
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
#[cfg(feature = "async-process")]
use windows_sys::Win32::Foundation::{
    DuplicateHandle, DUPLICATE_SAME_ACCESS, ERROR_INVALID_PARAMETER, HANDLE,
};
use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
#[cfg(feature = "async-process")]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
#[cfg(feature = "process-inspection")]
use windows_sys::Win32::System::Threading::GetProcessTimes;
#[cfg(feature = "async-process")]
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, TerminateProcess,
};

#[cfg(feature = "async-process")]
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
pub use cmdline::{read_process_argv, read_process_cmdline};

#[cfg(feature = "process-inspection")]
#[path = "platform/process_tree.rs"]
mod process_tree;

#[cfg(feature = "process-inspection")]
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
    configure_process_command_inner(command, config, false)
}

/// Root-facade-only launch seam for bounded owner-death containment.
///
/// This must be `pub` because `running-process` is a separate package, but
/// applications should use its semantic bounded-run options instead of this
/// implementation-detail function.
#[doc(hidden)]
pub fn configure_process_command_for_bounded_owner_death(
    command: &mut std::process::Command,
    config: crate::platform::process::ProcessCommandConfig,
) -> io::Result<()> {
    // NativeProcess already owns one per-spawn KILL_ON_JOB_CLOSE job. Do not
    // allocate or assign a second job for the bounded option.
    configure_process_command_inner(command, config, true)
}

fn configure_process_command_inner(
    command: &mut std::process::Command,
    config: crate::platform::process::ProcessCommandConfig,
    _kill_when_owner_dies: bool,
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
pub fn configure_sync_daemon_command_with_inheritance(
    _command: &mut std::process::Command,
    _inheritance: crate::platform::process::DaemonExecInheritance,
) -> io::Result<()> { Ok(()) }

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

#[cfg(feature = "async-process")]
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

#[cfg(feature = "async-process")]
fn compat_tokio_creation_flags(show_console: bool) -> u32 {
    if show_console {
        0
    } else {
        0x0800_0000 // CREATE_NO_WINDOW
    }
}

#[cfg(feature = "async-process")]
pub fn after_compat_tokio_spawn(child: &Child, kill_when_owner_dies: bool) -> io::Result<()> {
    if kill_when_owner_dies {
        assign(child.raw_handle())
    } else {
        Ok(())
    }
}

#[cfg(feature = "process-inspection")]
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

#[cfg(feature = "async-process")]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

#[cfg(feature = "async-process")]
pub(crate) fn configure_command(
    command: &mut Command,
    create_process_group: bool,
    _kill_when_owner_dies: bool,
    nice: Option<i32>,
) -> io::Result<()> {
    let group = if create_process_group {
        CREATE_NEW_PROCESS_GROUP
    } else {
        0
    };
    // Preserve the existing ProcessCommandConfig mapping: Windows receives a
    // priority *class*, not a Unix nice value with equivalent arithmetic.
    let priority = match nice {
        Some(value) if value >= 15 => 0x0000_0040,
        Some(value) if value >= 1 => 0x0000_4000,
        Some(value) if value <= -15 => 0x0000_0080,
        Some(value) if value <= -1 => 0x0000_8000,
        _ => 0,
    };
    if (group | priority) != 0 {
        command.creation_flags(group | priority);
    }
    Ok(())
}

#[cfg(feature = "async-process")]
pub(crate) fn after_spawn(child: &Child, kill_when_owner_dies: bool) -> io::Result<()> {
    if kill_when_owner_dies {
        assign(child.raw_handle())
    } else {
        Ok(())
    }
}

#[cfg(feature = "async-process")]
pub(crate) struct AsyncChildIdentity {
    pid: u32,
    process: HANDLE,
    creation_time: u64,
}

#[cfg(feature = "async-process")]
unsafe impl Send for AsyncChildIdentity {}

#[cfg(feature = "async-process")]
unsafe impl Sync for AsyncChildIdentity {}

#[cfg(feature = "async-process")]
impl Drop for AsyncChildIdentity {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.process) };
    }
}

#[cfg(feature = "async-process")]
pub(crate) fn async_child_identity(child: &Child) -> Option<AsyncChildIdentity> {
    let pid = child.id()?;
    let raw = child.raw_handle()? as HANDLE;
    let mut process = std::ptr::null_mut();
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            raw,
            GetCurrentProcess(),
            &mut process,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return None;
    }
    let Ok((creation_time, _, _)) = async_process_times(process) else {
        unsafe { CloseHandle(process) };
        return None;
    };
    Some(AsyncChildIdentity {
        pid,
        process,
        creation_time,
    })
}

#[cfg(feature = "async-process")]
pub(crate) fn signal_async_child(identity: &AsyncChildIdentity) -> io::Result<()> {
    if !identity_matches(identity) {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "child process launch identity no longer matches",
        ));
    }
    if unsafe { TerminateProcess(identity.process, 1) } != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(feature = "async-process")]
pub(crate) fn signal_async_child_group(identity: &AsyncChildIdentity) -> io::Result<()> {
    if !identity_matches(identity) {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "child process launch identity no longer matches",
        ));
    }
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, identity.pid) } != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_INVALID_HANDLE as i32) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(feature = "async-process")]
pub(crate) fn async_child_cpu_time(
    identity: &AsyncChildIdentity,
) -> io::Result<Option<std::time::Duration>> {
    let Ok((creation_time, user, kernel)) = async_process_times(identity.process) else {
        return Ok(None);
    };
    if creation_time != identity.creation_time {
        return Ok(None);
    }
    Ok(Some(std::time::Duration::from_nanos(
        user.saturating_add(kernel).saturating_mul(100),
    )))
}

#[cfg(feature = "async-process")]
fn identity_matches(identity: &AsyncChildIdentity) -> bool {
    const STILL_ACTIVE: u32 = 259;
    let mut exit_code = 0_u32;
    if unsafe { GetExitCodeProcess(identity.process, &mut exit_code) } == 0
        || exit_code != STILL_ACTIVE
    {
        return false;
    }
    matches!(
        async_process_times(identity.process),
        Ok((creation_time, _, _)) if creation_time == identity.creation_time
    )
}

#[cfg(feature = "async-process")]
fn async_process_times(process: HANDLE) -> io::Result<(u64, u64, u64)> {
    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    if unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let ticks = |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    Ok((ticks(creation), ticks(user), ticks(kernel)))
}

#[cfg(feature = "async-process")]
pub(crate) fn shell_spec(command: &OsStr) -> SpawnSpec {
    SpawnSpec::new("cmd.exe").arg("/C").arg(command)
}

// Only the async-process spawn paths place a child in the owner-death job;
// without that feature these items have no caller and fail `-D dead-code`.
#[cfg(feature = "async-process")]
struct Job(HANDLE);
#[cfg(feature = "async-process")]
unsafe impl Send for Job {}
#[cfg(feature = "async-process")]
unsafe impl Sync for Job {}

#[cfg(feature = "async-process")]
static JOB: OnceLock<Option<Job>> = OnceLock::new();

#[cfg(feature = "async-process")]
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

/// Place a freshly spawned child in the owner-death job.
///
/// Every step here used to fail silently, which is the wrong shape for this
/// operation: a caller passes `kill_when_owner_dies: true` precisely because
/// it does not want the child to outlive it, and a caller that is told
/// nothing cannot tell containment from its absence. All three failures are
/// now reported.
#[cfg(feature = "async-process")]
fn assign(child: Option<HANDLE>) -> io::Result<()> {
    let Some(child) = child else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot contain a child that exposes no process handle",
        ));
    };
    let Some(job) = JOB.get_or_init(create).as_ref() else {
        return Err(io::Error::other(
            "owner-death job object could not be created",
        ));
    };
    // SAFETY: `job.0` is the process-wide job handle and `child` is the
    // handle Tokio/std owns for the child just spawned.
    if unsafe { AssignProcessToJobObject(job.0, child) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
#[path = "platform_win/sync_spawn.rs"]
mod sync_spawn;
pub use sync_spawn::{spawn_sync, spawn_sync_daemon, spawn_sync_daemon_with_inheritance};

#[cfg(test)]
mod tests {
    #[cfg(feature = "async-process")]
    use super::compat_tokio_creation_flags;
    use std::ffi::OsStr;

    #[cfg(feature = "async-process")]
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

#[cfg(all(test, feature = "ipc"))]
mod endpoint_naming_tests {
    use super::{ipc_broker_v1_endpoint_path, ipc_endpoint_name_limit, WINDOWS_MAX_PATH};

    #[test]
    fn the_v1_address_is_a_named_pipe_carrying_the_bare_name() {
        let address = ipc_broker_v1_endpoint_path("rpb-v1-abc-shared").expect("derive address");
        assert!(address.starts_with(r"\\.\pipe\"));
        assert!(address.ends_with("rpb-v1-abc-shared"));
    }

    #[test]
    fn an_over_long_name_is_refused_against_max_path() {
        let err = ipc_broker_v1_endpoint_path(&"a".repeat(WINDOWS_MAX_PATH))
            .expect_err("must exceed MAX_PATH");
        assert_eq!(err.max, WINDOWS_MAX_PATH);
        assert_eq!(err.limit_label, "Windows MAX_PATH");
        assert!(err.len > WINDOWS_MAX_PATH);
    }

    #[test]
    fn the_reported_budget_is_max_path() {
        let limit = ipc_endpoint_name_limit();
        assert_eq!(limit.max_bytes, WINDOWS_MAX_PATH);
        assert_eq!(limit.label, "Windows MAX_PATH");
    }

    #[test]
    fn the_scope_spelling_folds_case_and_separators() {
        // Named pipes and paths are case-insensitive here, so two callers
        // spelling the same install differently must hash identically. This
        // pins the spelling itself: changing it re-scopes every deployed
        // broker, and the stability tests upstream would not notice.
        use super::ipc_endpoint_scope_bytes;

        let mixed = ipc_endpoint_scope_bytes(std::path::Path::new(r"C:\Program Files\App\Broker.exe"));
        assert_eq!(mixed, b"c:/program files/app/broker.exe".to_vec());

        let other = ipc_endpoint_scope_bytes(std::path::Path::new("c:/PROGRAM FILES/app/BROKER.exe"));
        assert_eq!(mixed, other);
    }

}

/// Replace this process's image with `command`.
///
/// Windows has no `execve`, so this always fails. It exists so the facade has
/// one shape on every host; callers check
/// [`can_replace_current_image`](crate::platform::process::can_replace_current_image)
/// first and start a successor instead.
pub fn process_replace_current_image(_command: &mut std::process::Command) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this host cannot replace a running process image",
    )
}

/// This host has no `execve`; a caller must start a successor and exit.
pub const fn process_can_replace_current_image() -> bool {
    false
}
