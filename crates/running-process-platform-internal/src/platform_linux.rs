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
