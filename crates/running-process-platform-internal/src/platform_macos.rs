//! macOS implementation root for the process capability.

#[cfg(feature = "ipc")]
#[path = "platform_macos/ipc.rs"]
pub(crate) mod ipc;
#[cfg(feature = "ipc")]
pub use ipc::{
    current_user_id as ipc_current_user_id, Endpoint as IpcEndpoint,
    InheritedListener as IpcInheritedListener, Listener as IpcListener,
    ListenerNonblockingMode as IpcListenerNonblockingMode, PeerIdentity as IpcPeerIdentity,
    PeerIdentitySource as IpcPeerIdentitySource, Stream as IpcStream,
};
#[cfg(feature = "ipc")]
pub fn ipc_broker_endpoint_name(bare_name: &str, path_scoped: bool) -> std::io::Result<String> {
    use std::fmt::Write as _;
    use std::path::PathBuf;

    let mut hash = blake3::Hasher::new();
    if path_scoped {
        hash.update(b"running-process:path-scoped-socket:v1\0");
        hash.update(bare_name.as_bytes());
        let mut leaf = String::with_capacity(32);
        for byte in hash.finalize().as_bytes().iter().take(16) { let _ = write!(leaf, "{byte:02x}"); }
        return Ok(PathBuf::from("/tmp").join(format!(".rp-path-{leaf}.sock")).to_string_lossy().into_owned());
    }
    hash.update(bare_name.as_bytes());
    let mut leaf = String::with_capacity(16);
    for byte in hash.finalize().as_bytes().iter().take(8) { let _ = write!(leaf, "{byte:02x}"); }
    let root = std::env::var_os("TMPDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    Ok(root.join(format!(".rp-{}-broker-v2", unsafe { libc::getuid() })).join(format!("{leaf}.sock")).to_string_lossy().into_owned())
}
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
#[path = "platform_macos_session_relay.rs"]
mod session_relay;
#[cfg(feature = "session-relay")]
pub use session_relay::relay_local_socket_session;

#[path = "platform_macos/terminal.rs"]
pub mod terminal;
pub use terminal::active_graphics_probe;
pub use crate::platform::terminal_input;

#[path = "platform_macos/window_icon.rs"]
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

#[path = "platform_macos_descendants.rs"]
mod descendants;
pub use descendants::start_descendant_monitor;

pub fn exact_trace_capability() -> crate::platform::process::ExactTraceCapability {
    crate::platform::process::ExactTraceCapability {
        available: false,
        backend: "macos-endpoint-security",
        reason: "exact recursive events require an entitled Endpoint Security provider",
        non_invasive_backend: "kqueue-proc-snapshot",
        non_invasive_grade:
            crate::platform::process::NonInvasiveObservationGrade::KernelHintReconciled,
    }
}

pub fn current_executable_build_id() -> Option<Vec<u8>> {
    None
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
        "Windows Job Objects are unavailable on macOS",
    ))
}

pub struct TracedChild(std::process::Child);

impl TracedChild {
    pub fn id(&self) -> u32 {
        self.0.id()
    }

    pub fn try_wait_code(&mut self) -> io::Result<Option<i32>> {
        self.0.try_wait().map(|status| status.map(exit_code))
    }

    pub fn kill(&mut self) -> io::Result<()> {
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
}

pub fn configure_exact_trace(_command: &mut std::process::Command) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        exact_trace_capability().reason,
    ))
}

pub fn start_exact_trace(
    _command: std::process::Command,
    _emit: Box<dyn Fn(crate::platform::process::ExactTraceEvent) + Send>,
    _complete: Box<dyn FnOnce() + Send>,
) -> io::Result<TracedChild> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        exact_trace_capability().reason,
    ))
}

#[derive(Default)]
pub struct CaptureCancellation { wakers: Mutex<CaptureWakers> }
#[derive(Default)]
struct CaptureWakers { stdout: Option<UnixStream>, stderr: Option<UnixStream> }
struct CancelableCaptureReader<R> { reader: R, wake_reader: UnixStream }
impl<R: Read + AsRawFd> Read for CancelableCaptureReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() { return Ok(0); }
        loop {
            let mut fds = [
                libc::pollfd { fd: self.reader.as_raw_fd(), events: libc::POLLIN | libc::POLLHUP | libc::POLLERR, revents: 0 },
                libc::pollfd { fd: self.wake_reader.as_raw_fd(), events: libc::POLLIN | libc::POLLHUP | libc::POLLERR, revents: 0 },
            ];
            // SAFETY: both descriptors remain owned by this reader for the call.
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) } < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted { continue; }
                return Err(error);
            }
            if fds[1].revents != 0 { return Err(io::Error::new(io::ErrorKind::Interrupted, "capture reader cancelled")); }
            if fds[0].revents != 0 {
                match self.reader.read(buf) {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                    result => return result,
                }
            }
        }
    }
}
pub fn prepare_capture_reader<R>(reader: R, cancellation: &CaptureCancellation, stream: crate::platform::process::CaptureStream) -> io::Result<Box<dyn Read + Send>>
where R: Read + AsRawFd + Send + 'static {
    set_nonblocking(reader.as_raw_fd())?;
    let (wake_reader, wake_writer) = UnixStream::pair()?;
    wake_writer.set_nonblocking(true)?;
    let mut wakers = cancellation.wakers.lock().expect("capture wakers mutex poisoned");
    match stream { crate::platform::process::CaptureStream::Stdout => wakers.stdout = Some(wake_writer), crate::platform::process::CaptureStream::Stderr => wakers.stderr = Some(wake_writer) }
    Ok(Box::new(CancelableCaptureReader { reader, wake_reader }))
}
pub fn capture_reader_done(cancellation: &CaptureCancellation, stream: crate::platform::process::CaptureStream) {
    let mut wakers = cancellation.wakers.lock().expect("capture wakers mutex poisoned");
    match stream { crate::platform::process::CaptureStream::Stdout => wakers.stdout = None, crate::platform::process::CaptureStream::Stderr => wakers.stderr = None }
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
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 { return Err(io::Error::last_os_error()); }
    Ok(())
}

#[path = "platform_macos_file_handles.rs"]
mod file_handles;
pub use file_handles::read_process_file_handles;
#[path = "platform_macos_cmdline.rs"]
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
    let c_name = std::ffi::CString::new(name).unwrap_or_default();
    unsafe { libc::pthread_setname_np(c_name.as_ptr()); }
}

pub fn configure_trampoline_command(_command: &mut std::process::Command) {}

pub fn configure_process_command(
    command: &mut std::process::Command,
    config: crate::platform::process::ProcessCommandConfig,
) -> io::Result<()> {
    let create_process_group = config.create_process_group;
    let nice = config.nice;
    if !(create_process_group || nice.is_some()) {
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
            Ok(())
        });
    }
    Ok(())
}

pub fn trampoline_exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map_or_else(|| status.code().unwrap_or(1), |signal| 128 + signal)
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

const PROC_ALL_PIDS: u32 = 1;
const PROC_PIDTBSDINFO: libc::c_int = 3;

pub fn process_snapshot() -> Vec<crate::platform::process::ProcessSnapshot> {
    let size = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if size <= 0 {
        return Vec::new();
    }
    let pid_count = (size as usize) / std::mem::size_of::<libc::pid_t>();
    if pid_count == 0 {
        return Vec::new();
    }
    let mut pids: Vec<libc::pid_t> = vec![0; pid_count];
    let written_bytes = unsafe {
        libc::proc_listpids(
            PROC_ALL_PIDS,
            0,
            pids.as_mut_ptr() as *mut libc::c_void,
            (pid_count * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        )
    };
    if written_bytes <= 0 {
        return Vec::new();
    }
    pids.truncate((written_bytes as usize) / std::mem::size_of::<libc::pid_t>());
    pids.into_iter()
        .filter(|pid| *pid > 0)
        .filter_map(|pid| process_snapshot_for_pid(pid as u32))
        .collect()
}

pub fn process_snapshot_for_pid(pid: u32) -> Option<crate::platform::process::ProcessSnapshot> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            PROC_PIDTBSDINFO,
            0,
            &mut info as *mut libc::proc_bsdinfo as *mut libc::c_void,
            std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int,
        )
    };
    (written as usize == std::mem::size_of::<libc::proc_bsdinfo>()).then_some(
        crate::platform::process::ProcessSnapshot {
            pid: info.pbi_pid,
            parent_pid: info.pbi_ppid,
            start_time_a: info.pbi_start_tvsec,
            start_time_b: info.pbi_start_tvusec,
        },
    )
}

/// Mark inherited descriptors close-on-exec without breaking std's exec-error pipe.
///
/// # Safety
/// This must only be called from a post-fork `pre_exec` closure.
pub unsafe fn unix_mark_extra_fds_close_on_exec() {
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
            unix_mark_extra_fds_close_on_exec();
            Ok(())
        });
    }
    Ok(())
}

pub fn parent_has_console() -> bool { false }

pub fn sync_child_native_handle(_child: &std::process::Child) -> usize { 0 }

unsafe fn set_cloexec(fd: libc::c_int) {
    let flags = libc::fcntl(fd, libc::F_GETFD);
    if flags != -1 { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC); }
}
pub fn observer_backend(scope: crate::platform::process::ObserverScope, category: crate::platform::process::ObserverCategory) -> crate::platform::process::ObserverBackend {
    use crate::platform::process::{ObserverBackend as B, ObserverCategory as C, ObserverScope as S, ObserverSupport as P};
    match (scope, category) {
        (S::SystemWide, C::File) => B { support:P::Unavailable, backend:"kqueue", reason:"Phase 3: macOS kqueue/EndpointSecurity file backend not yet implemented (entitlement-gated)" },
        (S::SystemWide, C::Network) | (S::SystemWide, C::Process) => B { support:P::Unavailable, backend:"endpoint-security", reason:"Phase 3: macOS EndpointSecurity backend not yet implemented (entitlement-gated)" },
        (S::LaunchedProcessTree, C::File) => B { support:P::Partial, backend:"proc-pidinfo", reason:"macOS proc_pidinfo(PROC_PIDLISTFDS) snapshot via read_process_file_handles (#539 slice 8 follow-up; no streaming file events)" },
        (S::LaunchedProcessTree, C::Network) => B { support:P::Unavailable, backend:"none", reason:"#539: no-admin per-child network backend deferred to a follow-up issue" },
        (S::LaunchedProcessTree, C::Process) => B { support:P::Supported, backend:"sysctl-proc-poll", reason:"macOS sysctl(KERN_PROC_ALL) descendant polling (#539 slice 7)" },
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
    let owner_pid = unsafe { libc::getpid() };
    configure_command_for_owner(
        command,
        create_process_group,
        kill_when_owner_dies,
        owner_pid,
    )
}

fn configure_command_for_owner(
    command: &mut Command,
    create_process_group: bool,
    kill_when_owner_dies: bool,
    owner_pid: libc::pid_t,
) -> io::Result<()> {
    if create_process_group {
        command.process_group(0);
    }
    if kill_when_owner_dies {
        // SAFETY: the closure and supervisor use only libc operations that do
        // not acquire process-global Rust state after Tokio forks the child.
        unsafe {
            command.pre_exec(move || install_owner_death_supervisor(owner_pid));
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
    if result == 0 { return Ok(()); }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) { Ok(()) } else { Err(error) }
}

pub(crate) fn shell_spec(command: &OsStr) -> SpawnSpec {
    SpawnSpec::new("/bin/sh").arg("-c").arg(command)
}

fn install_owner_death_supervisor(owner_pid: libc::pid_t) -> io::Result<()> {
    let mut handshake = [-1; 2];
    if unsafe { libc::pipe(handshake.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }

    let supervisor = unsafe { libc::fork() };
    if supervisor < 0 {
        let error = io::Error::last_os_error();
        unsafe {
            libc::close(handshake[0]);
            libc::close(handshake[1]);
        }
        return Err(error);
    }
    if supervisor == 0 {
        unsafe { libc::close(handshake[0]) };
        owner_death_supervisor(owner_pid, handshake[1]);
    }

    unsafe { libc::close(handshake[1]) };
    let result = read_supervisor_status(handshake[0]);
    unsafe { libc::close(handshake[0]) };
    result
}

fn read_supervisor_status(fd: libc::c_int) -> io::Result<()> {
    let mut status = 0_i32;
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            (&mut status as *mut i32).cast::<u8>(),
            std::mem::size_of::<i32>(),
        )
    };
    let mut offset = 0;
    while offset < bytes.len() {
        let read = unsafe {
            libc::read(
                fd,
                bytes[offset..].as_mut_ptr().cast(),
                bytes.len() - offset,
            )
        };
        if read > 0 {
            offset += read as usize;
            continue;
        }
        if read < 0 && last_errno() == libc::EINTR {
            continue;
        }
        return Err(io::Error::from_raw_os_error(if read == 0 {
            libc::EPIPE
        } else {
            last_errno()
        }));
    }
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status))
    }
}

fn owner_death_supervisor(
    owner_pid: libc::pid_t,
    handshake_fd: libc::c_int,
) -> ! {
    let target_pid = unsafe { libc::getppid() };
    if let Err(error) = close_supervisor_fds(handshake_fd) {
        report_supervisor_status(handshake_fd, error);
        unsafe { libc::_exit(127) };
    }
    let queue = unsafe { libc::kqueue() };
    if queue < 0 {
        report_supervisor_status(handshake_fd, last_errno());
        unsafe { libc::_exit(127) };
    }
    let mut watches = [
        libc::kevent { ident: owner_pid as libc::uintptr_t, filter: libc::EVFILT_PROC, flags: libc::EV_ADD | libc::EV_ONESHOT, fflags: libc::NOTE_EXIT, data: 0, udata: std::ptr::null_mut() },
        libc::kevent { ident: target_pid as libc::uintptr_t, filter: libc::EVFILT_PROC, flags: libc::EV_ADD | libc::EV_ONESHOT, fflags: libc::NOTE_EXIT, data: 0, udata: std::ptr::null_mut() },
    ];
    let registered = unsafe { libc::kevent(queue, watches.as_mut_ptr(), watches.len() as i32, std::ptr::null_mut(), 0, std::ptr::null()) };
    if registered < 0 {
        report_supervisor_status(handshake_fd, last_errno());
        unsafe {
            libc::close(queue);
            libc::_exit(127);
        }
    }
    if unsafe { libc::kill(owner_pid, 0) } < 0 && last_errno() == libc::ESRCH {
        report_supervisor_status(handshake_fd, libc::ESRCH);
        unsafe {
            libc::close(queue);
            libc::_exit(127);
        }
    }
    report_supervisor_status(handshake_fd, 0);
    unsafe { libc::close(handshake_fd) };

    let mut events = [unsafe { std::mem::zeroed::<libc::kevent>() }];
    loop {
        let count = unsafe { libc::kevent(queue, std::ptr::null(), 0, events.as_mut_ptr(), 1, std::ptr::null()) };
        if count <= 0 {
            if count < 0 && last_errno() == libc::EINTR { continue; }
            unsafe { libc::kill(target_pid, libc::SIGKILL); }
            break;
        }
        if events[0].ident == owner_pid as libc::uintptr_t { unsafe { libc::kill(target_pid, libc::SIGKILL); } }
        break;
    }
    unsafe { libc::close(queue); libc::_exit(0); }
}

fn close_supervisor_fds(handshake_fd: libc::c_int) -> Result<(), libc::c_int> {
    const BATCH_SIZE: usize = 64;
    // XNU's bsd/kern/syscalls.master assigns `proc_info` syscall 336, and
    // bsd/sys/proc_info_private.h assigns `PROC_INFO_CALL_PIDINFO` value 2.
    // Enter the kernel directly here: unlike libproc's `proc_pidinfo` wrapper,
    // this leaf syscall cannot acquire a process-global userspace lock after
    // the multithreaded owner has forked.
    const SYS_PROC_INFO: libc::c_int = 336;
    const PROC_INFO_CALL_PIDINFO: libc::c_int = 2;

    loop {
        let mut entries: [libc::proc_fdinfo; BATCH_SIZE] = unsafe { std::mem::zeroed() };
        let bytes = unsafe {
            libc::syscall(
                SYS_PROC_INFO,
                PROC_INFO_CALL_PIDINFO,
                libc::getpid(),
                libc::PROC_PIDLISTFDS,
                0_u64,
                entries.as_mut_ptr().cast::<libc::c_void>(),
                std::mem::size_of_val(&entries) as libc::c_int,
            )
        };
        if bytes <= 0 {
            let error = last_errno();
            return Err(if error == 0 { libc::EIO } else { error });
        }
        let bytes = bytes as usize;
        if bytes > std::mem::size_of_val(&entries) {
            return Err(libc::EOVERFLOW);
        }
        if !bytes.is_multiple_of(std::mem::size_of::<libc::proc_fdinfo>()) {
            return Err(libc::EIO);
        }
        let count = bytes / std::mem::size_of::<libc::proc_fdinfo>();
        if count == 0 {
            return Ok(());
        }

        let mut retained = 0;
        for entry in &entries[..count] {
            if entry.proc_fd == handshake_fd {
                retained += 1;
                continue;
            }
            if unsafe { libc::close(entry.proc_fd) } < 0 {
                let error = last_errno();
                if error != libc::EBADF {
                    return Err(error);
                }
            }
        }
        if retained == count {
            return Ok(());
        }
    }
}

fn report_supervisor_status(fd: libc::c_int, status: libc::c_int) {
    let bytes = status.to_ne_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        let written = unsafe {
            libc::write(
                fd,
                bytes[offset..].as_ptr().cast(),
                bytes.len() - offset,
            )
        };
        if written > 0 {
            offset += written as usize;
        } else if written < 0 && last_errno() == libc::EINTR {
            continue;
        } else {
            break;
        }
    }
}

fn last_errno() -> libc::c_int {
    unsafe { *libc::__error() }
}

#[cfg(test)]
#[path = "platform_macos_tests.rs"]
mod tests;

#[path = "sync_spawn_group.rs"]
mod sync_spawn;
pub use sync_spawn::{spawn_sync, spawn_sync_daemon};
