//! Caller-owned foreground command execution.
//!
//! Unlike contained, bounded, and daemon spawning, this module intentionally
//! does not configure process groups, sessions, descriptor inheritance,
//! consoles, owner-death policy, or environment. The supplied `Command` is
//! the complete native contract.

use std::io;
use std::process::{Child, Command, ExitStatus, Output};

/// Start a caller-configured command and transfer its native child ownership.
/// No containment or drop cleanup is added: the caller remains responsible for
/// consuming pipes, termination and reaping, exactly as with `Command::spawn`.
/// Use a contained/session API when that ownership policy is desired instead.
pub fn spawn(command: &mut Command) -> io::Result<Child> {
    command.spawn()
}

/// Run and return the native exit status.
///
/// Standard streams retain exactly the caller's `Command` configuration; the
/// std default is inherited streams, while explicit `stdin`/`stdout`/`stderr`
/// overrides remain in force.
pub fn status(command: &mut Command) -> io::Result<ExitStatus> {
    command.status()
}

/// Run with std's concurrent captured-output behavior and native exit status.
/// Its stdio behavior is exactly `Command::output`: unspecified stdout/stderr
/// are captured while explicit stream overrides remain caller-controlled.
/// Every other caller-owned command property remains unchanged.
pub fn output(command: &mut Command) -> io::Result<Output> {
    command.output()
}

/// Replace the current Unix process image using the supplied native command.
/// Success never returns; failure returns the original OS error. This does not
/// spawn a child or change the caller's requested argv, environment, stdio,
/// pre-exec hooks, process group, or session policy. As with `CommandExt::exec`,
/// failed execution may leave setup changes applied to the current process.
#[cfg(unix)]
pub fn exec(command: &mut Command) -> io::Error {
    use std::os::unix::process::CommandExt;
    command.exec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_preserves_native_missing_program_error() {
        let directory = tempfile::tempdir().expect("private fixture directory");
        let mut command = Command::new(directory.path().join("absent-executable"));
        let error = spawn(&mut command).expect_err("no child should be created");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[cfg(unix)]
    #[test]
    fn spawn_transfers_child_and_preserves_configured_output() {
        use std::io::{Read, Seek};
        use std::process::Stdio;
        use std::time::{Duration, Instant};
        let mut destination = tempfile::tempfile().expect("output file");
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "printf '%s' \"$MARKER\"; exit 7"])
            .env("MARKER", "caller-configured-output")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::from(destination.try_clone().unwrap()));
        let mut child = spawn(&mut command).expect("foreground spawn");
        assert!(
            child.stdout.is_none(),
            "file binding must not become a capture pipe"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                outcome => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("foreground fixture did not complete: {outcome:?}");
                }
            }
        };
        assert_eq!(status.code(), Some(7));
        destination.rewind().unwrap();
        let mut bytes = Vec::new();
        destination.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"caller-configured-output");
    }

    #[cfg(unix)]
    #[test]
    fn exec_returns_native_missing_image_error() {
        let directory = tempfile::tempdir().expect("private fixture directory");
        let mut command = Command::new(directory.path().join("absent-executable"));
        // No stdio/cwd/env/hook changes: the failed exec leaves this test's
        // process configuration untouched, and never runs another image.
        let error = exec(&mut command);
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
    }

    #[cfg(unix)]
    #[test]
    fn output_preserves_cwd_environment_literal_argv_and_native_nonzero() {
        let directory = tempfile::tempdir().expect("fixture cwd");
        let cwd = directory.path().canonicalize().expect("physical cwd");
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "[ \"$#\" = 2 ] && [ -z \"$2\" ] || exit 9; printf '%s\\n%s\\n%s\\n' \"$PWD\" \"$MARKER\" \"$1\"; exit 7", "foreground", "$literal; not a command", ""]);
        command.current_dir(&cwd).env("MARKER", "foreground-env");
        let output = output(&mut command).expect("foreground output");
        assert_eq!(output.status.code(), Some(7));
        let expected = format!(
            "{}\nforeground-env\n$literal; not a command\n",
            cwd.display()
        );
        assert_eq!(output.stdout, expected.as_bytes());
        assert!(output.stderr.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn output_preserves_windows_environment_cwd_and_nonzero_status() {
        let directory = tempfile::tempdir().expect("fixture cwd");
        let mut command = Command::new("cmd.exe");
        // /D disables user AutoRun commands; CD emits the actual child cwd.
        command.args(["/D", "/C", "cd & echo %MARKER% & exit /b 7"]);
        command
            .current_dir(directory.path())
            .env("MARKER", "foreground-env");
        let captured = output(&mut command).expect("foreground output");
        assert_eq!(captured.status.code(), Some(7));
        let text = String::from_utf8_lossy(&captured.stdout);
        let mut lines = text.lines();
        let actual = std::path::Path::new(lines.next().expect("cwd line"));
        assert_eq!(
            actual.canonicalize().unwrap(),
            directory.path().canonicalize().unwrap()
        );
        assert_eq!(
            lines.next().expect("environment line").trim(),
            "foreground-env"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn foreground_keeps_parent_process_group_and_session() {
        let parent = std::fs::read_to_string("/proc/self/stat").expect("parent stat");
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "cut -d' ' -f5,6 /proc/self/stat"]);
        let output = output(&mut command).expect("foreground child");
        let parent_fields: Vec<_> = parent
            .rsplit_once(") ")
            .expect("stat")
            .1
            .split_whitespace()
            .collect();
        let child = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            child.trim(),
            format!("{} {}", parent_fields[2], parent_fields[3])
        );
    }

    #[cfg(unix)]
    #[test]
    fn both_execution_forms_preserve_explicit_stdio_overrides() {
        use std::io::{Read, Seek, Write};
        use std::process::Stdio;
        for capture in [false, true] {
            let mut input = tempfile::tempfile().unwrap();
            input.write_all(b"caller-controlled input").unwrap();
            input.rewind().unwrap();
            let mut destination = tempfile::tempfile().unwrap();
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", "cat; exit 7"])
                .stdin(Stdio::from(input))
                .stdout(Stdio::from(destination.try_clone().unwrap()))
                .stderr(Stdio::null());
            if capture {
                let captured = output(&mut command).unwrap();
                assert_eq!(captured.status.code(), Some(7));
                assert!(captured.stdout.is_empty());
                assert!(captured.stderr.is_empty());
            } else {
                assert_eq!(status(&mut command).unwrap().code(), Some(7));
            }
            destination.rewind().unwrap();
            let mut bytes = Vec::new();
            destination.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"caller-controlled input");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn foreground_does_not_close_an_inheritable_high_descriptor() {
        use std::io::{Seek, Write};
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let mut file = tempfile::tempfile().expect("fixture file");
        file.write_all(b"inherited-resource").unwrap();
        file.rewind().unwrap();
        // SAFETY: F_DUPFD duplicates the live borrowed descriptor; it does
        // not access userspace memory or change the source descriptor.
        let descriptor = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 64) };
        assert!(descriptor >= 64);
        // SAFETY: F_DUPFD returned a distinct owned descriptor.
        let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "cat \"/proc/self/fd/$1\"",
            "foreground",
            &descriptor.as_raw_fd().to_string(),
        ]);
        let captured = output(&mut command).expect("inherited descriptor read");
        assert!(captured.status.success());
        assert_eq!(captured.stdout, b"inherited-resource");
    }
}
