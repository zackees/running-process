//! Sanitized Linux independent-spawn helper and scheduler CLI launch.

use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

/// Terminate a scheduler CLI without blocking its caller on Unix reaping.
///
/// A successful signal does not guarantee an immediately waitable process
/// (for example, one stuck in uninterruptible kernel I/O). Transfer ownership
/// to a reaper rather than extending a launch/cancellation deadline with wait.
/// This only cleans up the CLI; callers must separately cancel its service.
pub fn terminate_independent_scheduler_command(mut child: Child) -> io::Result<()> {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return Ok(());
    }
    let termination = child.kill();
    // Reap even if signaling failed: the process can still exit naturally.
    std::thread::Builder::new()
        .name("rp-scheduler-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })?;
    termination
}

/// Launch a decoded target or scheduler CLI with null stdio and sanitized descriptors.
///
/// This does not create independent cgroup placement. Only the external
/// manager/broker establishes placement; the returned ordinary child remains
/// waitable by its caller. Scheduler CLIs use this boundary too, so unrelated
/// caller descriptors cannot leak into the manager probe/submission/cleanup.
pub fn spawn_independent_helper_child(command: &mut Command) -> io::Result<Child> {
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: the closure invokes only the existing native post-fork descriptor
    // sanitizer. CLOEXEC preserves std's exec-error pipe until exec succeeds.
    unsafe {
        command.pre_exec(|| {
            super::unix_mark_extra_fds_close_on_exec();
            Ok(())
        });
    }
    command.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    #[test]
    fn scheduler_cleanup_accepts_an_already_reaped_child() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        let mut child = spawn_independent_helper_child(&mut command).unwrap();
        assert!(child.wait().unwrap().success());
        terminate_independent_scheduler_command(child).unwrap();
    }

    #[test]
    fn scheduler_cleanup_reaps_a_running_child_off_the_caller_path() {
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let child = spawn_independent_helper_child(&mut command).unwrap();
        let pid = child.id();
        terminate_independent_scheduler_command(child).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::path::Path::new(&format!("/proc/{pid}")).exists() {
            assert!(std::time::Instant::now() < deadline, "scheduler child was not reaped");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn helper_target_does_not_inherit_unmarked_descriptors() {
        let source = std::fs::File::open("/dev/null").unwrap();
        // F_DUPFD intentionally creates an inheritable descriptor. Allocate
        // a fresh descriptor rather than replacing any process-owned slot.
        let descriptor = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD, 128) };
        assert!(descriptor >= 0, "duplicate test descriptor: {}", io::Error::last_os_error());
        let inherited = unsafe { OwnedFd::from_raw_fd(descriptor) };
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "test ! -e /proc/self/fd/$RP_TEST_INHERITED_FD"])
            .env("RP_TEST_INHERITED_FD", descriptor.to_string());
        let status = spawn_independent_helper_child(&mut command).unwrap().wait().unwrap();
        assert!(status.success(), "target inherited a helper-owned descriptor");
        // Sanitization applies only after fork: the helper's own descriptor
        // must remain open and retain its original flags.
        let flags = unsafe { libc::fcntl(inherited.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0);
        assert_eq!(flags & libc::FD_CLOEXEC, 0);
    }
}
