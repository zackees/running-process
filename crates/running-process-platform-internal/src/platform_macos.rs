//! macOS implementation root for the process capability.

use std::ffi::OsStr;
use std::io;

use tokio::process::{Child, Command};

use crate::SpawnSpec;

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

pub fn trampoline_exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map_or_else(|| status.code().unwrap_or(1), |signal| 128 + signal)
}

pub fn enable_descendant_subreaper() {}
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
    if create_process_group {
        command.process_group(0);
    }
    if kill_when_owner_dies {
        let owner_pid = unsafe { libc::getpid() };
        // SAFETY: the helper is created before exec and owns the kqueue loop.
        unsafe {
            command.pre_exec(move || {
                let supervisor = libc::fork();
                if supervisor < 0 {
                    return Err(io::Error::last_os_error());
                }
                if supervisor == 0 {
                    owner_death_supervisor(owner_pid);
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
    if result == 0 { return Ok(()); }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) { Ok(()) } else { Err(error) }
}

pub(crate) fn shell_spec(command: &OsStr) -> SpawnSpec {
    SpawnSpec::new("/bin/sh").arg("-c").arg(command)
}

fn owner_death_supervisor(owner_pid: libc::pid_t) -> ! {
    let target_pid = unsafe { libc::getppid() };
    unsafe { for fd in 3..1024 { libc::close(fd); } }
    let queue = unsafe { libc::kqueue() };
    if queue < 0 { unsafe { libc::_exit(127) }; }
    let mut watches = [
        libc::kevent { ident: owner_pid as libc::uintptr_t, filter: libc::EVFILT_PROC, flags: libc::EV_ADD | libc::EV_ONESHOT, fflags: libc::NOTE_EXIT, data: 0, udata: std::ptr::null_mut() },
        libc::kevent { ident: target_pid as libc::uintptr_t, filter: libc::EVFILT_PROC, flags: libc::EV_ADD | libc::EV_ONESHOT, fflags: libc::NOTE_EXIT, data: 0, udata: std::ptr::null_mut() },
    ];
    let registered = unsafe { libc::kevent(queue, watches.as_mut_ptr(), watches.len() as i32, std::ptr::null_mut(), 0, std::ptr::null()) };
    if registered < 0 { unsafe { libc::close(queue); libc::_exit(127); } }
    if unsafe { libc::kill(owner_pid, 0) } < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        unsafe { libc::kill(target_pid, libc::SIGTERM); libc::close(queue); libc::_exit(0); }
    }
    let mut events = [unsafe { std::mem::zeroed::<libc::kevent>() }];
    loop {
        let count = unsafe { libc::kevent(queue, std::ptr::null(), 0, events.as_mut_ptr(), 1, std::ptr::null()) };
        if count <= 0 {
            if count < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) { continue; }
            break;
        }
        if events[0].ident == owner_pid as libc::uintptr_t { unsafe { libc::kill(target_pid, libc::SIGTERM); } }
        break;
    }
    unsafe { libc::close(queue); libc::_exit(0); }
}
