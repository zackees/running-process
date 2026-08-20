//! Linux implementation root for the process capability.

#[cfg(feature = "ipc")]
#[path = "platform_linux/ipc.rs"]
pub(crate) mod ipc;
#[cfg(feature = "ipc")]
pub use ipc::{
    current_user_id as ipc_current_user_id, Endpoint as IpcEndpoint,
    InheritedListener as IpcInheritedListener, Listener as IpcListener,
    ListenerNonblockingMode as IpcListenerNonblockingMode, PeerIdentity as IpcPeerIdentity,
    PeerIdentitySource as IpcPeerIdentitySource, Stream as IpcStream,
};
#[cfg(feature = "ipc")]
pub fn into_legacy_ipc_stream(stream: IpcStream) -> interprocess::local_socket::Stream {
    stream.0
}
#[cfg(feature = "ipc-async")]
pub use ipc::{
    AsyncListener as IpcAsyncListener, AsyncStream as IpcAsyncStream,
    IntoAsyncListener as IpcIntoAsyncListener, IntoAsyncStream as IpcIntoAsyncStream,
};

#[cfg(feature = "session-relay")]
#[path = "platform_linux_session_relay.rs"]
mod session_relay;
#[cfg(feature = "session-relay")]
pub use session_relay::relay_local_socket_session;

#[path = "platform_linux/terminal.rs"]
pub mod terminal;
pub use terminal::active_graphics_probe;
pub use crate::platform::terminal_input;

#[path = "platform_linux/window_icon.rs"]
mod window_icon;
pub use window_icon::{icon_support as window_icon_support_impl, set_icon as set_window_icon_impl};

pub fn shell_command(command: &str) -> std::process::Command {
    let mut shell = std::process::Command::new("/bin/sh");
    shell.arg("-lc").arg(command);
    shell
}

pub fn compat_shell_command(command: &str) -> std::process::Command {
    let mut shell = std::process::Command::new("/bin/sh");
    shell.arg("-lc").arg(command);
    shell
}

pub fn canonical_environment_pairs(pairs: Vec<(String, String)>) -> Vec<(String, String)> {
    pairs
}

pub fn monitor_console_windows(
    _duration: std::time::Duration,
) -> Vec<crate::platform::process::ConsoleWindowInfo> {
    Vec::new()
}

use std::ffi::OsStr;
use std::io;
use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;

use tokio::process::{Child, Command};

use crate::SpawnSpec;

#[path = "platform_linux_descendants.rs"]
mod descendants;
pub use descendants::start_descendant_monitor;

#[path = "platform_linux_trace.rs"]
mod exact_trace;
pub use exact_trace::{configure_exact_trace, start_exact_trace, TracedChild};

pub fn exact_trace_capability() -> crate::platform::process::ExactTraceCapability {
    crate::platform::process::ExactTraceCapability {
        available: true,
        backend: "linux-ptrace",
        reason: "launch-time PTRACE_TRACEME with follow-fork/clone/exec/exit supervision",
        non_invasive_backend: "proc-descendant-snapshot",
        non_invasive_grade:
            crate::platform::process::NonInvasiveObservationGrade::SnapshotInferred,
    }
}

pub struct WindowsJobHandle;

pub fn assign_child_to_windows_job(
    _child: &std::process::Child,
    _direct_pid: u32,
    _address_space_limit_bytes: Option<u64>,
    _emit: Option<Box<dyn Fn(crate::platform::process::DescendantEvent) + Send>>,
) -> io::Result<WindowsJobHandle> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows Job Objects are unavailable on Linux",
    ))
}

#[derive(Default)]
pub struct CaptureCancellation {
    wakers: Mutex<CaptureWakers>,
}

#[derive(Default)]
struct CaptureWakers {
    stdout: Option<UnixStream>,
    stderr: Option<UnixStream>,
}

struct CancelableCaptureReader<R> {
    reader: R,
    wake_reader: UnixStream,
}

impl<R: Read + AsRawFd> Read for CancelableCaptureReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() { return Ok(0); }
        loop {
            let mut poll_fds = [
                libc::pollfd { fd: self.reader.as_raw_fd(), events: libc::POLLIN | libc::POLLHUP | libc::POLLERR, revents: 0 },
                libc::pollfd { fd: self.wake_reader.as_raw_fd(), events: libc::POLLIN | libc::POLLHUP | libc::POLLERR, revents: 0 },
            ];
            // SAFETY: both descriptors remain owned by this reader for the call.
            let polled = unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as _, -1) };
            if polled < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted { continue; }
                return Err(error);
            }
            if poll_fds[1].revents != 0 {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "capture reader cancelled"));
            }
            if poll_fds[0].revents != 0 {
                match self.reader.read(buf) {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                    result => return result,
                }
            }
        }
    }
}

pub fn prepare_capture_reader<R>(
    reader: R,
    cancellation: &CaptureCancellation,
    stream: crate::platform::process::CaptureStream,
) -> io::Result<Box<dyn Read + Send>>
where R: Read + AsRawFd + Send + 'static {
    set_nonblocking(reader.as_raw_fd())?;
    let (wake_reader, wake_writer) = UnixStream::pair()?;
    wake_writer.set_nonblocking(true)?;
    let mut wakers = cancellation.wakers.lock().expect("capture wakers mutex poisoned");
    match stream {
        crate::platform::process::CaptureStream::Stdout => wakers.stdout = Some(wake_writer),
        crate::platform::process::CaptureStream::Stderr => wakers.stderr = Some(wake_writer),
    }
    Ok(Box::new(CancelableCaptureReader { reader, wake_reader }))
}

pub fn capture_reader_done(cancellation: &CaptureCancellation, stream: crate::platform::process::CaptureStream) {
    let mut wakers = cancellation.wakers.lock().expect("capture wakers mutex poisoned");
    match stream {
        crate::platform::process::CaptureStream::Stdout => wakers.stdout = None,
        crate::platform::process::CaptureStream::Stderr => wakers.stderr = None,
    }
}

pub fn cancel_capture_reader(cancellation: &CaptureCancellation) {
    let wakers = cancellation.wakers.lock().expect("capture wakers mutex poisoned");
    let byte = [1_u8; 1];
    for writer in [&wakers.stdout, &wakers.stderr].into_iter().flatten() {
        // SAFETY: the stored wake socket stays alive while the mutex is held.
        let _ = unsafe { libc::write(writer.as_raw_fd(), byte.as_ptr().cast(), byte.len()) };
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` is borrowed from a live reader for both calls.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 { return Err(io::Error::last_os_error()); }
    // SAFETY: `fd` is borrowed from a live reader for both calls.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[path = "platform_linux_file_handles.rs"]
mod file_handles;
pub use file_handles::read_process_file_handles;
#[path = "platform_linux_cmdline.rs"]
mod cmdline;
pub use cmdline::read_process_cmdline;

#[path = "platform/process_tree.rs"]
mod process_tree;

pub fn kill_tree(pid: u32, timeout: std::time::Duration) -> io::Result<u32> {
    process_tree::kill_tree(pid, timeout, |_pid, process| Ok(process.start_time()))
}

pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| -status.signal().unwrap_or(1))
}

pub fn set_process_name(name: &str) {
    let truncated: String = name.chars().take(15).collect();
    let c_name = std::ffi::CString::new(truncated).unwrap_or_default();
    unsafe { libc::prctl(libc::PR_SET_NAME, c_name.as_ptr() as libc::c_ulong, 0, 0, 0); }
}

pub fn configure_trampoline_command(_command: &mut std::process::Command) {}

pub fn configure_process_command(
    command: &mut std::process::Command,
    config: crate::platform::process::ProcessCommandConfig,
) -> io::Result<()> {
    let create_process_group = config.create_process_group;
    let nice = config.nice;
    let address_space_limit_bytes = config.address_space_limit_bytes;
    if !(create_process_group || nice.is_some() || address_space_limit_bytes.is_some()) {
        return Ok(());
    }
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(move || {
            if create_process_group && libc::setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            if let Some(nice) = nice {
                if libc::setpriority(libc::PRIO_PROCESS, 0, nice) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            if let Some(limit) = address_space_limit_bytes {
                let rlim = libc::rlimit { rlim_cur: limit, rlim_max: limit };
                if libc::setrlimit(libc::RLIMIT_AS, &rlim) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(())
}

pub fn trampoline_exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map_or_else(|| status.code().unwrap_or(1), |signal| 128 + signal)
}

/// Return the GNU build ID of the running executable without reading the
/// executable from disk.
///
/// The dynamic loader has already mapped the main image's `PT_NOTE` segment,
/// so callers that only need an image-generation identity do not need to hash
/// a potentially large unoptimized binary. `None` preserves a clean fallback
/// for binaries linked without a GNU build ID.
pub fn current_executable_build_id() -> Option<Vec<u8>> {
    unsafe extern "C" fn visit(
        info: *mut libc::dl_phdr_info,
        _size: libc::size_t,
        output: *mut libc::c_void,
    ) -> libc::c_int {
        const MAX_NOTE_BYTES: usize = 1024 * 1024;

        let info = unsafe { &*info };
        let is_main_executable = info.dlpi_name.is_null()
            || unsafe { std::ffi::CStr::from_ptr(info.dlpi_name) }
                .to_bytes()
                .is_empty();
        if !is_main_executable || info.dlpi_phdr.is_null() || info.dlpi_phnum == 0 {
            return 0;
        }
        let headers = unsafe {
            std::slice::from_raw_parts(info.dlpi_phdr, usize::from(info.dlpi_phnum))
        };
        #[allow(clippy::unnecessary_cast)]
        let load_bias = info.dlpi_addr as u64;
        for header in headers {
            if header.p_type != libc::PT_NOTE {
                continue;
            }
            let Ok(length) = usize::try_from(header.p_memsz) else {
                continue;
            };
            if length == 0 || length > MAX_NOTE_BYTES {
                continue;
            }
            let Some(address) = load_bias.checked_add(header.p_vaddr) else {
                continue;
            };
            let Some(note_end) = address.checked_add(length as u64) else {
                continue;
            };
            let mapped_read_only = headers.iter().any(|load| {
                if load.p_type != libc::PT_LOAD || load.p_flags & libc::PF_R == 0 {
                    return false;
                }
                let Some(start) = load_bias.checked_add(load.p_vaddr) else {
                    return false;
                };
                let Some(end) = start.checked_add(load.p_memsz) else {
                    return false;
                };
                address >= start && note_end <= end
            });
            if address == 0 || !mapped_read_only {
                continue;
            }
            let notes = unsafe { std::slice::from_raw_parts(address as *const u8, length) };
            if let Some(build_id) = gnu_build_id_from_notes(notes) {
                let output = unsafe { &mut *output.cast::<Option<Vec<u8>>>() };
                *output = Some(build_id.to_vec());
                return 1;
            }
        }
        0
    }

    let mut output = None;
    unsafe {
        libc::dl_iterate_phdr(
            Some(visit),
            (&mut output as *mut Option<Vec<u8>>).cast::<libc::c_void>(),
        );
    }
    output
}

fn gnu_build_id_from_notes(mut notes: &[u8]) -> Option<&[u8]> {
    fn aligned(value: usize) -> Option<usize> {
        value.checked_add(3).map(|value| value & !3)
    }

    while notes.len() >= 12 {
        let name_len = usize::try_from(u32::from_ne_bytes(notes[0..4].try_into().ok()?)).ok()?;
        let desc_len = usize::try_from(u32::from_ne_bytes(notes[4..8].try_into().ok()?)).ok()?;
        let kind = u32::from_ne_bytes(notes[8..12].try_into().ok()?);
        let name_end = 12usize.checked_add(name_len)?;
        let desc_start = 12usize.checked_add(aligned(name_len)?)?;
        let desc_end = desc_start.checked_add(desc_len)?;
        let next = desc_start.checked_add(aligned(desc_len)?)?;
        if next > notes.len() || name_end > notes.len() || desc_end > notes.len() {
            return None;
        }
        if kind == 3 && notes.get(12..name_end)?.starts_with(b"GNU") && desc_len > 0 {
            return notes.get(desc_start..desc_end);
        }
        notes = &notes[next..];
    }
    None
}

/// Request a graceful shutdown for a child-owned POSIX process group.
pub fn soft_terminate_process_group(pid: u32) -> io::Result<()> {
    // SAFETY: `kill` receives only the numeric child-owned group id; no Rust
    // references or borrowed state cross the OS boundary.
    let result = unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
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

/// Mark inherited descriptors close-on-exec without breaking std's exec-error pipe.
///
/// # Safety
/// This must only be called from a post-fork `pre_exec` closure.
pub unsafe fn unix_mark_extra_fds_close_on_exec() {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "x86", target_arch = "arm", target_arch = "riscv64", target_arch = "powerpc64"))]
    {
        const SYS_CLOSE_RANGE: libc::c_long = 436;
        const CLOSE_RANGE_CLOEXEC: libc::c_uint = 4;
        if libc::syscall(SYS_CLOSE_RANGE, 3u32, libc::c_uint::MAX, CLOSE_RANGE_CLOEXEC) == 0 {
            return;
        }
    }
    mark_fds_from_directory_or_range();
}

pub fn configure_sync_daemon_command(command: &mut std::process::Command) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            let _ = libc::setsid();
            unix_mark_extra_fds_close_on_exec();
            Ok(())
        });
    }
    Ok(())
}

pub fn configure_sync_contained_command(command: &mut std::process::Command) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 { return Err(io::Error::last_os_error()); }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() == 1 { libc::_exit(1); }
            unix_mark_extra_fds_close_on_exec();
            Ok(())
        });
    }
    Ok(())
}

pub fn parent_has_console() -> bool { false }

pub fn sync_child_native_handle(_child: &std::process::Child) -> usize { 0 }

unsafe fn mark_fds_from_directory_or_range() {
    let dir = libc::opendir(c"/dev/fd".as_ptr());
    if !dir.is_null() {
        let dir_fd = libc::dirfd(dir);
        loop {
            let entry = libc::readdir(dir);
            if entry.is_null() { break; }
            let mut fd: libc::c_int = 0;
            let mut cursor = (*entry).d_name.as_ptr();
            let mut numeric = false;
            while *cursor != 0 {
                let byte = *cursor as u8;
                if !byte.is_ascii_digit() { numeric = false; break; }
                fd = fd * 10 + (byte - b'0') as libc::c_int;
                cursor = cursor.add(1);
                numeric = true;
            }
            if numeric && fd > 2 && fd != dir_fd { set_cloexec(fd); }
        }
        libc::closedir(dir);
        return;
    }
    let maximum = libc::sysconf(libc::_SC_OPEN_MAX);
    for fd in 3..if maximum < 0 { 4096 } else { maximum as libc::c_int } { set_cloexec(fd); }
}

unsafe fn set_cloexec(fd: libc::c_int) {
    let flags = libc::fcntl(fd, libc::F_GETFD);
    if flags != -1 { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC); }
}
pub fn observer_backend(scope: crate::platform::process::ObserverScope, category: crate::platform::process::ObserverCategory) -> crate::platform::process::ObserverBackend {
    use crate::platform::process::{ObserverBackend as B, ObserverCategory as C, ObserverScope as S, ObserverSupport as P};
    match (scope, category) {
        (S::SystemWide, C::File) => B { support:P::Unavailable, backend:"seccomp-user-notify", reason:"Phase 3: Linux seccomp user-notify file backend not yet implemented" },
        (S::SystemWide, C::Network) => B { support:P::Unavailable, backend:"ebpf", reason:"Phase 3: Linux eBPF network backend not yet implemented" },
        (S::SystemWide, C::Process) => B { support:P::Unavailable, backend:"seccomp-user-notify", reason:"Phase 3: Linux seccomp user-notify process backend not yet implemented" },
        (S::LaunchedProcessTree, C::File) => B { support:P::Partial, backend:"proc-fd-snapshot", reason:"Linux /proc/<pid>/fd/* snapshot via read_process_file_handles (#539 slice 6 follow-up; no streaming file events)" },
        (S::LaunchedProcessTree, C::Network) => B { support:P::Unavailable, backend:"none", reason:"#539: no-admin per-child network backend deferred to a follow-up issue" },
        (S::LaunchedProcessTree, C::Process) => B { support:P::Supported, backend:"subreaper-proc-poll", reason:"Linux PR_SET_CHILD_SUBREAPER + /proc descendant polling (#539 slice 5)" },
    }
}

pub fn unix_set_priority(pid: u32, nice: i32) -> io::Result<()> {
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, pid, nice) } == -1 { Err(io::Error::last_os_error()) } else { Ok(()) }
}
pub fn unix_signal_process(pid: u32, signal: crate::platform::process::UnixSignalKind) -> io::Result<()> {
    if unsafe { libc::kill(pid as i32, unix_signal_raw(signal)) } == -1 { Err(io::Error::last_os_error()) } else { Ok(()) }
}
pub fn unix_signal_process_group(pid: i32, signal: crate::platform::process::UnixSignalKind) -> io::Result<()> {
    if unsafe { libc::killpg(pid, unix_signal_raw(signal)) } == -1 { Err(io::Error::last_os_error()) } else { Ok(()) }
}
pub fn unix_signal_raw(signal: crate::platform::process::UnixSignalKind) -> i32 {
    match signal { crate::platform::process::UnixSignalKind::Interrupt => libc::SIGINT, crate::platform::process::UnixSignalKind::Terminate => libc::SIGTERM, crate::platform::process::UnixSignalKind::Kill => libc::SIGKILL }
}

pub fn configure_compat_tokio_command(
    command: &mut Command,
    _show_console: bool,
    kill_when_owner_dies: bool,
) -> io::Result<()> {
    configure_command(command, false, kill_when_owner_dies)
}

pub fn after_compat_tokio_spawn(_child: &Child, _kill_when_owner_dies: bool) {}

pub(crate) fn configure_command(
    command: &mut Command,
    create_process_group: bool,
    kill_when_owner_dies: bool,
) -> io::Result<()> {
    if create_process_group {
        command.process_group(0);
    }
    if kill_when_owner_dies {
        let owner_pid = unsafe { libc::getpid() };
        // SAFETY: the closure invokes only async-signal-safe libc calls.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(
                    libc::PR_SET_PDEATHSIG,
                    libc::SIGTERM as libc::c_ulong,
                    0,
                    0,
                    0,
                ) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != owner_pid {
                    libc::kill(libc::getpid(), libc::SIGTERM);
                }
                Ok(())
            });
        }
    }
    Ok(())
}

pub(crate) fn after_spawn(_child: &Child, _kill_when_owner_dies: bool) {}

pub(crate) fn signal_process(pid: u32) -> io::Result<()> {
    unix_kill(pid as i32, libc::SIGKILL)
}

pub(crate) fn signal_process_group(pid: u32) -> io::Result<()> {
    unix_kill(-(pid as i32), libc::SIGTERM)
}

fn unix_kill(target: i32, signal: i32) -> io::Result<()> {
    let result = unsafe { libc::kill(target, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

pub(crate) fn shell_spec(command: &OsStr) -> SpawnSpec {
    SpawnSpec::new("/bin/sh").arg("-c").arg(command)
}

#[cfg(test)]
mod tests {
    #[test]
    fn shell_command_preserves_login_shell_contract_and_ignores_child_path() {
        use std::ffi::OsStr;

        let command_text = "printf '%s' 'alpha beta;\"gamma\"'";
        let mut command = super::shell_command(command_text);
        assert_eq!(command.get_program(), OsStr::new("/bin/sh"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [OsStr::new("-lc"), OsStr::new(command_text)]
        );
        command
            .env_clear()
            .env("PATH", "/caller-supplied-path-override");
        let output = command
            .output()
            .expect("absolute shell command should execute independently of child PATH");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"alpha beta;\"gamma\"");
    }

    #[test]
    #[cfg(not(target_env = "musl"))]
    fn current_executable_exposes_a_gnu_build_id() {
        let build_id = super::current_executable_build_id()
            .expect("Linux test executable should carry a GNU build ID");
        assert!(!build_id.is_empty());
    }
}
#[cfg(test)]
#[path = "tests/platform_linux_coverage.rs"]
mod coverage_tests;
#[path = "sync_spawn_group.rs"]
mod sync_spawn;
pub use sync_spawn::{spawn_sync, spawn_sync_daemon};
