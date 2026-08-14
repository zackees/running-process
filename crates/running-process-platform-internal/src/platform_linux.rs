//! Linux implementation root for the process capability.

use std::ffi::OsStr;
use std::io;

use tokio::process::{Child, Command};

use crate::SpawnSpec;

#[path = "platform/process_tree.rs"]
mod process_tree;

pub fn kill_tree(pid: u32, timeout: std::time::Duration) -> io::Result<u32> {
    process_tree::kill_tree(pid, timeout, |_pid, process| Ok(process.start_time()))
}

pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| -status.signal().unwrap_or(1))
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
