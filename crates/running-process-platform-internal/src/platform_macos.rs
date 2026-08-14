//! macOS implementation root for the process capability.

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
