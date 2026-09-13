//! Asking this host about another process (Linux).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;

use crate::platform::process::{ProcessInspectError, ProcessInspectErrorKind};

/// Linux process creation identity in clock ticks since boot. Callers needing
/// reuse-safe control must retain a pidfd as well; this is not a signal handle.
pub fn process_start_key(pid: u32) -> io::Result<u64> {
    use std::io::Read as _;
    validate_pid(pid).map_err(|error| error.source)?;
    const LIMIT: u64 = 64 * 1024;
    let mut stat = String::new();
    std::fs::File::open(format!("/proc/{pid}/stat"))?
        .take(LIMIT + 1).read_to_string(&mut stat)?;
    if stat.len() as u64 > LIMIT {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "oversized process identity record"));
    }
    parse_start_key(&stat)
}

fn parse_start_key(stat: &str) -> io::Result<u64> {
    // comm may contain spaces and closing parentheses. The final delimiter
    // precedes field3; starttime is field22, hence index19 in this suffix.
    stat.rsplit_once(") ").and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing or invalid process start identity"))
}

#[cfg(test)]
mod start_key_tests {
    use super::*;

    #[test]
    fn parses_starttime_after_a_complex_process_name() {
        let fields = [vec!["S"], vec!["0"; 18], vec!["123456"]].concat().join(" ");
        assert_eq!(parse_start_key(&format!("42 (worker ) with spaces) {fields} 99\n")).unwrap(), 123456);
    }

    #[test]
    fn malformed_or_truncated_identity_is_not_zero() {
        for stat in ["", "42 worker S 0", "42 (worker) S 0 0"] {
            assert_eq!(parse_start_key(stat).unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
        assert!(process_start_key(0).is_err());
        assert!(process_start_key(u32::MAX).is_err());
    }
}

/// A strict, reuse-safe process reference. Unlike `ProcessLiveness`, this
/// never falls back to numeric PID operations when pidfds are unavailable.
#[derive(Debug)]
pub struct StrictProcessHandle(OwnedFd);

impl StrictProcessHandle {
    pub fn open(pid: u32) -> io::Result<Self> {
        validate_pid(pid).map_err(|error| error.source)?;
        // SAFETY: validated PID and zero flags; success returns an owned fd.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0_u32) };
        if raw < 0 { return Err(io::Error::last_os_error()); }
        Ok(Self(unsafe { OwnedFd::from_raw_fd(raw as i32) }))
    }

    /// Check signal permission without delivering a signal.
    pub fn check_signal_permission(&self) -> io::Result<()> { self.signal(0) }

    pub fn kill(&self) -> io::Result<()> {
        match self.signal(libc::SIGKILL) {
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
            result => result,
        }
    }

    fn signal(&self, signal: libc::c_int) -> io::Result<()> {
        // SAFETY: owned pidfd, signal value, no siginfo, zero flags.
        let result = unsafe { libc::syscall(libc::SYS_pidfd_send_signal,
            self.0.as_raw_fd(), signal, std::ptr::null::<libc::siginfo_t>(), 0_u32) };
        if result == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }

    pub fn has_exited(&self) -> io::Result<bool> {
        let mut descriptor = libc::pollfd { fd: self.0.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid writable pollfd, zero timeout.
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result < 0 { return Err(io::Error::last_os_error()); }
        if descriptor.revents & (libc::POLLNVAL | libc::POLLERR) != 0 {
            return Err(io::Error::other("invalid process handle readiness"));
        }
        Ok(descriptor.revents & (libc::POLLIN | libc::POLLHUP) != 0)
    }
}

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
    /// Acquire reuse-safe control without falling back to numeric PID signals.
    pub fn open_for_control(pid: u32) -> Result<Self, ProcessInspectError> {
        validate_pid(pid)?;
        let handle = StrictProcessHandle::open(pid).map_err(|source| ProcessInspectError {
            kind: ProcessInspectErrorKind::Host,
            source,
        })?;
        handle.check_signal_permission().map_err(|source| ProcessInspectError {
            kind: ProcessInspectErrorKind::Host,
            source,
        })?;
        Ok(Self { pid, pid_fd: Some(handle.0) })
    }

    /// Force termination through the held pidfd only; never reopen the PID.
    pub fn force_kill(&self) -> io::Result<()> {
        let fd = self.pid_fd.as_ref().ok_or_else(|| io::Error::new(
            io::ErrorKind::Unsupported, "process control requires a held pidfd",
        ))?;
        // SAFETY: held descriptor, fixed signal, no siginfo and zero flags.
        let result = unsafe { libc::syscall(libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(), libc::SIGKILL, std::ptr::null::<libc::siginfo_t>(), 0_u32) };
        if result == 0 { return Ok(()); }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) { Ok(()) } else { Err(error) }
    }

    /// Confirm terminal state through the held pidfd, preserving poll errors.
    /// Numeric-PID observation is not a substitute for cleanup confirmation.
    pub fn has_exited(&self) -> io::Result<bool> {
        let fd = self.pid_fd.as_ref().ok_or_else(|| io::Error::new(
            io::ErrorKind::Unsupported, "exit confirmation requires a held pidfd",
        ))?;
        let mut poll_fd = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one initialized descriptor; no blocking wait.
        if unsafe { libc::poll(&mut poll_fd, 1, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if poll_fd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(io::Error::other("held pidfd poll failed"));
        }
        Ok(poll_fd.revents & libc::POLLIN != 0)
    }

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

#[cfg(test)]
mod held_control_tests {
    use super::*;

    #[test]
    fn force_kill_never_falls_back_to_a_live_numeric_pid() {
        // A regression would signal this test process. The intentionally absent
        // pidfd must instead reject control before attempting any syscall.
        let handle = ProcessLiveness { pid: std::process::id(), pid_fd: None };
        let error = handle.force_kill().expect_err("numeric fallback is forbidden");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        let error = handle.has_exited().expect_err("no held object can confirm exit");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
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

    #[test]
    fn strict_handles_reject_non_process_identifiers() {
        assert!(StrictProcessHandle::open(0).is_err());
        assert!(StrictProcessHandle::open(u32::MAX).is_err());
    }

    #[test]
    fn strict_self_handle_is_live_and_supports_signal_zero_when_available() {
        let handle = match StrictProcessHandle::open(std::process::id()) {
            Ok(handle) => handle,
            Err(error) if matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EPERM | libc::EACCES)) => return,
            Err(error) => panic!("unexpected pidfd open error: {error}"),
        };
        assert!(!handle.has_exited().expect("query self pidfd"));
        match handle.check_signal_permission() {
            Ok(()) => (),
            Err(error) if matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EPERM | libc::EACCES)) => (),
            Err(error) => panic!("unexpected pidfd signal-zero error: {error}"),
        }
    }

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
