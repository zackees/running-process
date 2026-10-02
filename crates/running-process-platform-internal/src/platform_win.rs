//! Windows implementation root for the process capability.
#[cfg(feature = "independent-spawn")]
#[path = "independent_broker_unsupported.rs"]
mod independent_broker;
#[cfg(feature = "independent-spawn")]
pub use independent_broker::{run as independent_broker_run, spawn as independent_broker_spawn};
#[cfg(feature = "independent-spawn")]
mod independent_spawn;
#[cfg(feature = "independent-spawn")]
mod scheduler_error;
#[cfg(feature = "independent-spawn")]
mod scheduler_launch;
#[cfg(feature = "independent-spawn")]
pub use independent_spawn::{spawn as independent_spawn, IndependentChild};
#[cfg(feature = "independent-spawn")]
mod independent_io;
#[cfg(feature = "independent-spawn")]
pub(crate) use independent_io::open_regular as independent_open_regular;
#[cfg(feature = "independent-spawn")]
pub(crate) const INDEPENDENT_ZERO_WRITE_PENDING: bool = true;

#[path = "platform_win/foreground.rs"]
pub(crate) mod foreground;

/// Native niceness for the portable `ProcessPriority::Low` intent.
pub(crate) const PRIORITY_NICE_LOW: i32 = 1;
/// Native niceness for the portable `ProcessPriority::High` intent.
pub(crate) const PRIORITY_NICE_HIGH: i32 = -15;

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
    process_fault_code_name, process_signal_terminate, ProcessLiveness,
};

#[path = "platform_win/loaded_images.rs"]
mod loaded_images;
pub use loaded_images::{
    loaded_images as process_loaded_images,
    open_loaded_image_file as process_open_loaded_image_file,
};

#[path = "platform_win/raw_write.rs"]
pub(crate) mod raw_write;
pub use raw_write::write_all_to_descriptor as fs_write_all_to_descriptor;

/// Whether a handle another process holds open keeps a file from being removed.
///
/// Windows refuses to remove a file another process holds open, so
/// callers use the answer to decide whether releasing handles before a
/// recursive delete means anything here.
pub const fn fs_open_handles_block_removal() -> bool {
    true
}

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

/// Windows has no control groups.
pub fn host_process_cgroup() -> Option<io::Result<String>> {
    None
}

#[cfg(feature = "fs")]
#[path = "platform_win/fs.rs"]
pub(crate) mod fs;
#[cfg(feature = "fs")]
pub use fs::{
    create_private_file as fs_create_private_file,
    is_link_handle as fs_is_link_handle, open_read_no_follow as fs_open_read_no_follow,
    decode_path_bytes as fs_decode_path_bytes,
    replace_file as fs_replace_file, sync_directory as fs_sync_directory,
    user_config_dir as fs_user_config_dir,
    user_data_dir as fs_user_data_dir, encode_path_bytes as fs_encode_path_bytes,
    file_identity as fs_file_identity, is_lock_conflict as fs_is_lock_conflict,
    open_lock_file as fs_open_lock_file, path_identity as fs_path_identity,
    try_lock_exclusive as fs_try_lock_exclusive, unlock as fs_unlock,
    user_run_data_root as fs_user_run_data_root, user_runtime_dir as fs_user_runtime_dir,
    user_state_dir as fs_user_state_dir,
    user_state_dir_from_environment as fs_user_state_dir_from_environment,
    state_home_from_environment as fs_state_home_from_environment,
    FileIdentity as FsFileIdentity,
};

#[path = "platform_win/ape.rs"]
pub(crate) mod ape;
pub use ape::{
    default_loader_dirs as ape_default_loader_dirs, is_exec_format_error as ape_is_exec_format_error,
    is_executable as ape_is_executable, mark_executable as ape_mark_executable,
    anonymous_executable as ape_anonymous_executable,
    private_exec_dir as ape_private_exec_dir,
    route_through_execvp as ape_route_through_execvp, APE_LOADER_HOST,
    APE_EXECVP_SHELL_FALLBACK, APE_NEEDS_LOADER, APE_SHELL, APE_SYSTEM_LOADERS,
};
#[cfg(feature = "async-process")]
pub use ape::route_tokio_through_execvp as ape_route_tokio_through_execvp;

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
    handoff_transport_available as ipc_handoff_transport_available,
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
    Ok(ipc_component_endpoint_path("broker-v2", bare_name))
}

/// Named-pipe path for `bare_name`. The kernel pipe namespace is already
/// per-machine and name-keyed, so `component` needs no directory of its own;
/// callers keep their services apart with the name prefix (`rpp-probe-...`).
#[cfg(feature = "ipc")]
pub fn ipc_component_endpoint_path(_component: &str, bare_name: &str) -> String {
    format!(r"\\.\pipe\{bare_name}")
}

/// Per-user runtime directory of `component`, for runtime files a service
/// publishes (its pipes need none). `LOCALAPPDATA` is per-user and
/// non-roaming; the per-user temp directory stands in when it is unset. Pure.
#[cfg(feature = "ipc")]
pub fn ipc_component_runtime_dir(component: &str) -> std::path::PathBuf {
    component_runtime_dir_in(
        crate::env_vars::LOCALAPPDATA.os(),
        std::env::temp_dir(),
        component,
    )
}

#[cfg(feature = "ipc")]
fn component_runtime_dir_in(
    local_app_data: Option<std::ffi::OsString>,
    temp_dir: std::path::PathBuf,
    component: &str,
) -> std::path::PathBuf {
    local_app_data
        .map(std::path::PathBuf::from)
        .unwrap_or(temp_dir)
        .join("running-process")
        .join(component)
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

#[cfg(feature = "window-icon")]
#[path = "platform_win/window_icon.rs"]
mod window_icon;
#[cfg(feature = "window-icon")]
pub use window_icon::{icon_support as window_icon_support_impl, set_icon as set_window_icon_impl};

#[path = "platform_win_descendants.rs"]
mod descendants;
pub use descendants::{assign_child_to_windows_job, WindowsJobHandle};

/// Attach to an already-running root by polling the process table (#1015).
pub use descendants::start_snapshot_descendant_monitor as start_attached_descendant_monitor;

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
use tokio::process::{Child, Command};
// Each import carries the gate its users carry, so a build without
// `async-process` does not import symbols nothing references. Only
// `ERROR_INVALID_HANDLE` and the console pair have ungated users
// (`soft_terminate_process_group`); everything else here is reached solely
// from the async spawn/identity paths. `process_start_key` looks like a
// second consumer of `CloseHandle` / `FILETIME` / `GetProcessTimes` but
// imports all three itself, function-locally.
use windows_sys::Win32::Foundation::ERROR_INVALID_HANDLE;
#[cfg(feature = "async-process")]
use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, ERROR_INVALID_PARAMETER, FILETIME, HANDLE,
};
use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
#[cfg(feature = "async-process")]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
#[cfg(feature = "async-process")]
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, TerminateProcess,
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

#[cfg(feature = "async-process")]
#[path = "platform_win_output.rs"]
mod output_shutdown;
#[cfg(feature = "async-process")]
pub(crate) use output_shutdown::shutdown_output_reader;

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

/// The signal that terminated `status`'s process. Windows processes do not
/// die from signals, so there is never one to report.
pub fn exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
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

/// Apply a process priority expressed as a Unix nice value by mapping it to
/// the nearest Windows priority class.
pub fn apply_process_priority(pid: u32, nice: i32) -> io::Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, SetPriorityClass, ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS,
        HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
        PROCESS_QUERY_INFORMATION, PROCESS_SET_INFORMATION,
    };
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
    // SAFETY: OpenProcess takes plain values; the returned handle is closed
    // below on every path that obtains one.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_SET_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `handle` is a live process handle owned by this function.
    let set_ok = unsafe { SetPriorityClass(handle, priority_class) };
    // SAFETY: `handle` is closed exactly once.
    let close_ok = unsafe { CloseHandle(handle) };
    if close_ok == 0 || set_ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Windows `CREATE_NEW_PROCESS_GROUP`.
const CREATE_NEW_PROCESS_GROUP_FLAG: u32 = 0x0000_0200;

/// Deliver Ctrl+Break to the child-owned console process group `pid`.
///
/// Windows can only target a process group, so the child must have been
/// created with `CREATE_NEW_PROCESS_GROUP`; `create_process_group` is a Unix
/// notion and is ignored here.
pub fn send_interrupt(pid: u32, creationflags: Option<u32>, _create_process_group: bool) -> io::Result<()> {
    if creationflags.unwrap_or(0) & CREATE_NEW_PROCESS_GROUP_FLAG == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "send_interrupt on Windows requires CREATE_NEW_PROCESS_GROUP",
        ));
    }
    // SAFETY: the Windows API receives only a numeric process-group id.
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn unix_set_priority(_pid: u32, _nice: i32) -> io::Result<()> { Err(io::Error::new(io::ErrorKind::Unsupported, "Unix priority is unavailable on Windows")) }
pub fn unix_signal_process(_pid: u32, _signal: crate::platform::process::UnixSignalKind) -> io::Result<()> { Err(io::Error::new(io::ErrorKind::Unsupported, "Unix signals are unavailable on Windows")) }
pub fn unix_signal_process_group(_pid: i32, _signal: crate::platform::process::UnixSignalKind) -> io::Result<()> { Err(io::Error::new(io::ErrorKind::Unsupported, "Unix signals are unavailable on Windows")) }
pub fn unix_signal_raw(_signal: crate::platform::process::UnixSignalKind) -> i32 { 0 }

#[cfg(feature = "async-process")]
pub fn configure_compat_tokio_command(
    command: &mut Command,
    show_console: bool,
    kill_when_owner_dies: bool,
) -> io::Result<()> {
    let flags = compat_tokio_creation_flags(show_console, kill_when_owner_dies);
    if flags != 0 {
        command.creation_flags(flags);
    }
    Ok(())
}

#[cfg(feature = "async-process")]
const CREATE_SUSPENDED: u32 = 0x0000_0004;

/// `CREATE_SUSPENDED` when the child must join the owner-death job before it
/// runs a single instruction (#887). Assigning it after `CreateProcess` returns
/// leaves a window in which the child can start a grandchild that never joins
/// the job.
#[cfg(feature = "async-process")]
fn compat_tokio_creation_flags(show_console: bool, kill_when_owner_dies: bool) -> u32 {
    let console = if show_console {
        0
    } else {
        0x0800_0000 // CREATE_NO_WINDOW
    };
    let suspended = if kill_when_owner_dies { CREATE_SUSPENDED } else { 0 };
    console | suspended
}

/// Put a child spawned `CREATE_SUSPENDED` into the owner-death job, then let it
/// run.
///
/// If containment or the resume fails, the child is terminated: it has not run
/// a single instruction, so killing it is strictly safer than letting it run
/// outside the job.
#[cfg(feature = "async-process")]
fn contain_and_resume(child: &Child) -> io::Result<()> {
    let process = child.raw_handle();
    let outcome = assign(process).and_then(|()| match child.id() {
        Some(pid) => resume_primary_thread(pid),
        None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the child exited before it could be resumed",
        )),
    });
    if outcome.is_err() {
        if let Some(process) = process {
            // SAFETY: `process` is the live handle Tokio owns for this child.
            unsafe { TerminateProcess(process, 1) };
        }
    }
    outcome
}

/// Resume the one thread of a process created `CREATE_SUSPENDED`.
///
/// A freshly created suspended process has exactly one thread, so the first
/// thread owned by `pid` is the primary thread. Uses documented Win32 calls
/// only; std and Tokio do not expose the primary-thread handle.
#[cfg(feature = "async-process")]
fn resume_primary_thread(pid: u32) -> io::Result<()> {
    use winapi::um::handleapi::{CloseHandle as CloseWinHandle, INVALID_HANDLE_VALUE};
    use winapi::um::processthreadsapi::{OpenThread, ResumeThread};
    use winapi::um::tlhelp32::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use winapi::um::winnt::THREAD_SUSPEND_RESUME;

    // SAFETY: a plain snapshot call; the handle is closed on every path below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: THREADENTRY32 is plain data; `dwSize` is set before first use.
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut outcome = Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no thread found for the suspended child",
    ));
    // SAFETY: `snapshot` is live and `entry` is initialised as required.
    let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: opening a thread by id; the handle is closed right after.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                outcome = Err(io::Error::last_os_error());
            } else {
                // SAFETY: `thread` is a live handle with suspend/resume access.
                let previous = unsafe { ResumeThread(thread) };
                outcome = if previous == u32::MAX {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                };
                // SAFETY: closes the handle opened above.
                unsafe { CloseWinHandle(thread) };
            }
            break;
        }
        // SAFETY: as for `Thread32First`.
        more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    // SAFETY: closes the snapshot created above.
    unsafe { CloseWinHandle(snapshot) };
    outcome
}

#[cfg(feature = "async-process")]
pub fn after_compat_tokio_spawn(child: &Child, kill_when_owner_dies: bool) -> io::Result<()> {
    if kill_when_owner_dies {
        contain_and_resume(child)
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
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A console-less parent (e.g. a daemon) makes Windows give every child its
/// own visible console unless `CREATE_NO_WINDOW` is set.
#[cfg(feature = "async-process")]
fn spawn_creation_flags(
    group: u32,
    priority: u32,
    parent_has_console: bool,
    kill_when_owner_dies: bool,
) -> u32 {
    let no_window = if parent_has_console { 0 } else { CREATE_NO_WINDOW };
    // Owner-death children start suspended and are resumed once they are in the
    // job (#887); see `contain_and_resume`.
    let suspended = if kill_when_owner_dies { CREATE_SUSPENDED } else { 0 };
    group | priority | no_window | suspended
}

/// Configure a caller-built command for [`crate::SpawnSpec::from_std_command`].
///
/// Windows owner-death containment for these commands is the per-spawn
/// kill-on-close Job Object (with its descendant observer and memory limit)
/// that `NativeProcess` assigns after spawn. `SpawnSpec` cannot express that
/// job yet (#850), so the combination is refused rather than launched
/// uncontained.
#[cfg(feature = "async-process")]
pub(crate) fn configure_override_command(
    command: &mut std::process::Command,
    config: crate::platform::process::ProcessCommandConfig,
    kill_when_owner_dies: bool,
) -> io::Result<()> {
    if kill_when_owner_dies {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "owner-death containment of a caller-built command needs the per-spawn Job Object, \
             which SpawnSpec cannot express yet",
        ));
    }
    configure_process_command(command, config)
}

#[cfg(feature = "async-process")]
pub(crate) fn configure_command(
    command: &mut Command,
    create_process_group: bool,
    kill_when_owner_dies: bool,
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
    let flags = spawn_creation_flags(group, priority, parent_has_console(), kill_when_owner_dies);
    if flags != 0 {
        command.creation_flags(flags);
    }
    Ok(())
}

#[cfg(feature = "async-process")]
pub(crate) fn after_spawn(
    child: &Child,
    kill_when_owner_dies: bool,
    _nice: Option<i32>,
) -> io::Result<()> {
    if kill_when_owner_dies {
        contain_and_resume(child)
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

// Only the async-process spawn paths place a child in an owner-death job;
// without that feature these items have no caller and fail `-D dead-code`.
#[cfg(feature = "async-process")]
struct Job(HANDLE);
#[cfg(feature = "async-process")]
unsafe impl Send for Job {}

#[cfg(feature = "async-process")]
impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle came from `CreateJobObjectW` and is owned here.
        unsafe { CloseHandle(self.0) };
    }
}

/// Owner-death jobs that may still contain a live process.
///
/// Every contained child gets its own empty job. One process-wide job stops
/// accepting children once the owner itself joins another job: Windows assigns
/// a process only to a job that is empty or already in its job chain, so a
/// populated shared job rejects later children with `ERROR_ACCESS_DENIED`
/// (#1207). A job is released only after it has no active processes, because
/// closing a kill-on-close job earlier would terminate what it contains. The
/// remaining handles close when the owner exits, which is the containment.
#[cfg(feature = "async-process")]
static JOBS: Mutex<Vec<Job>> = Mutex::new(Vec::new());

#[cfg(feature = "async-process")]
fn create() -> io::Result<Job> {
    // SAFETY: both arguments may be null: default security and an unnamed job.
    let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let job = Job(handle);
    // SAFETY: an all-zero extended limit structure is valid; only the
    // kill-on-close flag is then set.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `info` is valid for the length passed, and the class matches it.
    let set = unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if set == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

/// Whether releasing `job` could still terminate a process.
#[cfg(feature = "async-process")]
fn job_may_contain_processes(job: &Job) -> bool {
    use windows_sys::Win32::System::JobObjects::{
        JobObjectBasicAccountingInformation, QueryInformationJobObject,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    };

    // SAFETY: an all-zero accounting structure is valid to overwrite.
    let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is valid for the length passed, and the class matches it.
    let queried = unsafe {
        QueryInformationJobObject(
            job.0,
            JobObjectBasicAccountingInformation,
            (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    // A job that cannot be queried is kept rather than risk killing a child.
    queried == 0 || info.ActiveProcesses != 0
}

/// Place a freshly spawned child in its own owner-death job.
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
    let job = create().map_err(|error| {
        io::Error::other(format!("owner-death job object could not be created: {error}"))
    })?;
    // SAFETY: `job.0` is the live job created above and `child` is the handle
    // Tokio/std owns for the child just spawned. On failure the job is closed
    // without containing anything.
    if unsafe { AssignProcessToJobObject(job.0, child) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut jobs = JOBS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    jobs.retain(job_may_contain_processes);
    jobs.push(job);
    Ok(())
}
#[path = "platform_win/sync_spawn.rs"]
mod sync_spawn;
pub use sync_spawn::{spawn_sync, spawn_sync_daemon, spawn_sync_daemon_with_inheritance};
#[cfg(feature = "independent-spawn")]
pub(crate) use sync_spawn::spawn_sync_owned_daemon;

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

#[cfg(test)]
mod tests {
    #[cfg(feature = "async-process")]
    use super::compat_tokio_creation_flags;
    use std::ffi::OsStr;

    #[cfg(feature = "async-process")]
    #[test]
    fn tokio_spawn_owns_console_creation_flags() {
        assert_eq!(compat_tokio_creation_flags(false, false), 0x0800_0000);
        assert_eq!(compat_tokio_creation_flags(true, false), 0);
    }

    #[cfg(feature = "async-process")]
    #[test]
    fn owner_death_children_start_suspended_so_they_join_the_job_before_running() {
        const CREATE_SUSPENDED: u32 = 0x0000_0004;
        assert_eq!(compat_tokio_creation_flags(false, true) & CREATE_SUSPENDED, CREATE_SUSPENDED);
        assert_eq!(compat_tokio_creation_flags(true, true), CREATE_SUSPENDED);
        assert_eq!(compat_tokio_creation_flags(false, false) & CREATE_SUSPENDED, 0);
        assert_eq!(super::spawn_creation_flags(0, 0, true, true), CREATE_SUSPENDED);
        assert_eq!(super::spawn_creation_flags(0, 0, true, false), 0);
    }

    #[cfg(feature = "async-process")]
    #[test]
    fn console_less_parent_spawns_children_without_a_window() {
        use super::{spawn_creation_flags, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

        // #1214: a parent with no console (a daemon) must not let Windows give
        // every child its own visible console.
        assert_eq!(spawn_creation_flags(0, 0, false, false), CREATE_NO_WINDOW);
        assert_eq!(
            spawn_creation_flags(CREATE_NEW_PROCESS_GROUP, 0x0000_0040, false, false),
            CREATE_NEW_PROCESS_GROUP | 0x0000_0040 | CREATE_NO_WINDOW
        );
        assert_eq!(spawn_creation_flags(0, 0, true, false), 0);
    }

    #[cfg(feature = "async-process")]
    fn cmd_child(script: &str, kill_when_owner_dies: bool) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("cmd.exe");
        command.args(["/c", script]).kill_on_drop(true);
        super::configure_compat_tokio_command(&mut command, false, kill_when_owner_dies).unwrap();
        command
    }

    #[cfg(feature = "async-process")]
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[cfg(feature = "async-process")]
    #[test]
    fn suspended_owner_death_child_is_resumed_and_runs_to_its_own_exit_code() {
        runtime().block_on(async {
            let mut child = cmd_child("exit 7", true).spawn().unwrap();
            super::after_compat_tokio_spawn(&child, true).unwrap();
            let status = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait())
                .await
                .expect("a child left suspended would hang here")
                .unwrap();
            assert_eq!(status.code(), Some(7));
        });
    }

    #[cfg(feature = "async-process")]
    #[test]
    fn owner_death_child_is_in_a_job_by_the_time_spawn_returns() {
        use windows_sys::Win32::System::JobObjects::IsProcessInJob;

        runtime().block_on(async {
            let mut child = cmd_child("ping -n 6 127.0.0.1 > nul", true).spawn().unwrap();
            super::after_compat_tokio_spawn(&child, true).unwrap();
            let mut in_job = 0;
            // SAFETY: the handle is the live child handle Tokio owns; a null job
            // asks whether the process is in any job.
            let ok = unsafe {
                IsProcessInJob(child.raw_handle().unwrap(), std::ptr::null_mut(), &mut in_job)
            };
            assert_ne!(ok, 0, "IsProcessInJob failed");
            assert_ne!(in_job, 0, "the child must already be contained");
            child.kill().await.unwrap();
        });
    }

    #[cfg(feature = "async-process")]
    #[test]
    fn non_owner_death_child_is_not_suspended_or_contained() {
        runtime().block_on(async {
            let mut child = cmd_child("exit 3", false).spawn().unwrap();
            super::after_compat_tokio_spawn(&child, false).unwrap();
            let status = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait())
                .await
                .expect("an ordinary child must run without being resumed")
                .unwrap();
            assert_eq!(status.code(), Some(3));
        });
    }

    #[test]
    fn fault_code_names_are_byte_exact() {
        // #974 PR 2: moved out of probe-daemon's crash store, spelling unchanged.
        assert_eq!(super::process_fault_code_name(0xC000_0005), "0xC0000005");
        assert_eq!(super::process_fault_code_name(0x8000_0003), "0x80000003");
    }

    #[cfg(feature = "ipc")]
    #[test]
    fn component_runtime_dir_is_byte_exact_for_the_probe() {
        // #974 PR 2: probe-daemon's discovery directory, formerly derived in
        // the daemon from `LOCALAPPDATA` with the temp directory as fallback.
        assert_eq!(
            super::component_runtime_dir_in(
                Some(std::ffi::OsString::from(r"C:\Users\u\AppData\Local")),
                std::path::PathBuf::from(r"C:\Temp"),
                "probe",
            ),
            std::path::PathBuf::from(r"C:\Users\u\AppData\Local\running-process\probe")
        );
        assert_eq!(
            super::component_runtime_dir_in(None, std::path::PathBuf::from(r"C:\Temp"), "probe"),
            std::path::PathBuf::from(r"C:\Temp\running-process\probe")
        );
    }

    #[cfg(feature = "ipc")]
    #[test]
    fn component_endpoint_path_is_the_bare_pipe_name_whatever_the_component() {
        // #974: the pipe namespace is machine-wide and name-keyed, so the
        // component needs no directory; services stay apart by name prefix.
        assert_eq!(
            super::ipc_component_endpoint_path("probe", "rpp-probe-abc-0"),
            r"\\.\pipe\rpp-probe-abc-0"
        );
        assert_eq!(
            super::ipc_component_endpoint_path("broker-v2", "rpb-v2-x-0"),
            r"\\.\pipe\rpb-v2-x-0"
        );
        assert_eq!(
            super::ipc_broker_endpoint_name("rpb-v2-x-0", false).unwrap(),
            super::ipc_component_endpoint_path("broker-v2", "rpb-v2-x-0")
        );
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

/// Pins the per-host answers that facade callers branch on, so a change to
/// either is a visible, reviewed edit rather than a silent behaviour change.
#[cfg(test)]
mod host_semantics_tests {
    const ABSENT_PID: u32 = 0x7fff_fffe;

    #[test]
    fn priority_on_absent_pid_reports_the_os_error() {
        assert!(super::apply_process_priority(ABSENT_PID, 0).is_err());
    }

    #[test]
    fn interrupt_requires_a_new_process_group() {
        for flags in [None, Some(0)] {
            let error = super::send_interrupt(1, flags, true).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert_eq!(
                error.to_string(),
                "send_interrupt on Windows requires CREATE_NEW_PROCESS_GROUP"
            );
        }
    }

    #[test]
    fn open_handles_block_removal_matches_this_host() {
        assert!(super::fs_open_handles_block_removal());
    }

    #[cfg(feature = "ipc")]
    #[test]
    fn handoff_transport_is_available() {
        assert!(super::ipc_handoff_transport_available());
    }
}
