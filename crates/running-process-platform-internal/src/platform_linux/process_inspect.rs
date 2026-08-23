//! Asking this host about another process (Linux).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;

use crate::platform::process::{ProcessInspectError, ProcessInspectErrorKind};

/// A live reference to another process, good for as long as it is held.
///
/// Where the kernel offers one, this holds a pidfd: a PID can be recycled
/// between two questions, but a pidfd cannot, so a handle opened once keeps
/// naming the process it was opened for even after that process exits. Older
/// kernels have no such thing, and there the handle falls back to asking
/// about the PID -- which is the best this host can do, not an equivalent.
pub struct ProcessLiveness {
    pid: u32,
    pid_fd: Option<OwnedFd>,
}

impl std::fmt::Debug for ProcessLiveness {
    /// Names the process, not the handle.
    ///
    /// The underlying descriptor or handle value is an artefact of this
    /// process's own table; printing it invites a reader to compare two
    /// numbers that were never comparable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessLiveness")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl ProcessLiveness {
    /// Take a reference to `pid`, failing if no such process is running.
    pub fn open(pid: u32) -> Result<Self, ProcessInspectError> {
        validate_pid(pid)?;
        if !process_exists(pid) {
            return Err(not_found());
        }
        Ok(Self {
            pid,
            pid_fd: try_pidfd_open(pid)?,
        })
    }

    /// The process ID this handle was opened for.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether that process is still running.
    pub fn is_alive(&self) -> bool {
        match self.pid_fd.as_ref() {
            Some(pid_fd) => pidfd_is_alive(pid_fd),
            None => process_exists(self.pid),
        }
    }
}

/// Resolve the on-disk image a running process was started from.
pub fn process_executable_path(pid: u32) -> Result<PathBuf, io::Error> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
}

/// Ask a process to stop.
pub fn process_signal_terminate(pid: u32) -> Result<(), ProcessInspectError> {
    signal(pid, libc::SIGTERM)
}

/// Stop a process without asking.
pub fn process_force_kill(pid: u32) -> Result<(), ProcessInspectError> {
    signal(pid, libc::SIGKILL)
}

fn signal(pid: u32, signal: libc::c_int) -> Result<(), ProcessInspectError> {
    let native_pid = validate_pid(pid)?;
    // SAFETY: `native_pid` is in range and the signal number is a constant.
    let rc = unsafe { libc::kill(native_pid, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(ProcessInspectError::last_os_error(
            ProcessInspectErrorKind::Host,
        ))
    }
}

/// Signal zero: the permission and existence checks run, nothing is delivered.
///
/// `EPERM` counts as alive. A process we are not allowed to signal is still a
/// process, and reporting it dead would invite a caller to reuse its PID.
fn process_exists(pid: u32) -> bool {
    let Ok(native_pid) = validate_pid(pid) else {
        return false;
    };
    // SAFETY: `native_pid` is in range; signal 0 delivers nothing.
    let rc = unsafe { libc::kill(native_pid, 0) };
    if rc == 0 {
        return true;
    }
    matches!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM))
}

fn validate_pid(pid: u32) -> Result<libc::pid_t, ProcessInspectError> {
    if pid == 0 || pid > libc::pid_t::MAX as u32 {
        Err(ProcessInspectError::stated(
            ProcessInspectErrorKind::InvalidPid,
            "pid outside the range this host issues",
        ))
    } else {
        Ok(pid as libc::pid_t)
    }
}

/// Open a pidfd, or report that this kernel will not give us one.
///
/// A kernel without the syscall, a seccomp filter that hides it, and a denial
/// are all the same answer to the caller: no pidfd, fall back to the PID.
/// Only `ESRCH` is different -- that is the process being gone, which is
/// worth failing on rather than falling back to asking about a dead PID.
fn try_pidfd_open(pid: u32) -> Result<Option<OwnedFd>, ProcessInspectError> {
    // SAFETY: the syscall takes a pid and a flags word, both passed by value.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0_u32) };
    if raw >= 0 {
        // SAFETY: the syscall succeeded, so `raw` is a fresh descriptor this
        // handle now solely owns.
        return Ok(Some(unsafe { OwnedFd::from_raw_fd(raw as i32) }));
    }

    match io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Err(not_found()),
        _ => Ok(None),
    }
}

/// A pidfd becomes readable exactly when its process exits.
fn pidfd_is_alive(pid_fd: &OwnedFd) -> bool {
    let mut poll_fd = libc::pollfd {
        fd: pid_fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one initialised pollfd is described, and the zero timeout makes
    // this a poll rather than a wait.
    let rc = unsafe { libc::poll(&mut poll_fd, 1, 0) };
    rc == 0
}

fn not_found() -> ProcessInspectError {
    ProcessInspectError::stated(ProcessInspectErrorKind::NotFound, "no such process")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PID zero names no process on any host, and is rejected before the
    /// kernel is asked -- signal(0, ...) would mean "the whole process group".
    #[test]
    fn pid_zero_is_never_valid() {
        let error = ProcessLiveness::open(0).expect_err("pid 0");
        assert_eq!(error.kind, ProcessInspectErrorKind::InvalidPid);
        assert!(!process_exists(0));
    }

    /// This process is alive, and knows where it was started from.
    #[test]
    fn this_process_is_alive_and_locatable() {
        let me = std::process::id();
        let handle = ProcessLiveness::open(me).expect("open self");
        assert_eq!(handle.pid(), me);
        assert!(handle.is_alive());
        assert_eq!(
            process_executable_path(me).expect("exe"),
            std::env::current_exe().expect("current_exe")
        );
    }

    /// A handle keeps naming the process it was opened for. With a pidfd the
    /// kernel guarantees this; without one, the PID could in principle be
    /// recycled, which is exactly why the pidfd is preferred.
    #[test]
    fn a_dead_process_reports_dead() {
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn");
        let pid = child.id();
        let handle = ProcessLiveness::open(pid).expect("open child");
        let mut child = child;
        child.wait().expect("reap");
        assert!(!handle.is_alive(), "a reaped child must report dead");
    }
}

/// Whether two spellings name the same executable image on this host.
///
/// This host's paths are case-sensitive and distinguish nothing else, so once
/// both sides are resolved the comparison is exact. A path that cannot be
/// canonicalised is compared as written rather than treated as a mismatch,
/// because "the file moved" and "the caller lacks permission to resolve it"
/// arrive here identically.
pub fn process_same_executable_path(actual: &std::path::Path, expected: &std::path::Path) -> bool {
    let resolve =
        |path: &std::path::Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    resolve(actual) == resolve(expected)
}

#[cfg(test)]
mod path_tests {
    use super::*;
    use std::path::Path;

    /// Case is meaningful here; two spellings that differ by it are two files.
    #[test]
    fn case_distinguishes_two_images() {
        assert!(!process_same_executable_path(
            Path::new("/tmp/Daemon"),
            Path::new("/tmp/daemon"),
        ));
    }

    /// A path resolves to itself, canonicalisable or not.
    #[test]
    fn a_path_matches_itself() {
        assert!(process_same_executable_path(
            Path::new("/tmp/rp-does-not-exist/daemon"),
            Path::new("/tmp/rp-does-not-exist/daemon"),
        ));
        let me = std::env::current_exe().expect("current_exe");
        assert!(process_same_executable_path(&me, &me));
    }
}
