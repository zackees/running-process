//! macOS foreground execution: Unix process-image replacement.

use std::io;
use std::process::Command;

/// Replace the current Unix process image using the supplied native command.
/// Success never returns; failure returns the original OS error. This does not
/// spawn a child or change the caller's requested argv, environment, stdio,
/// pre-exec hooks, process group, or session policy. As with `CommandExt::exec`,
/// failed execution may leave setup changes applied to the current process.
pub fn exec(command: &mut Command) -> io::Error {
    use std::os::unix::process::CommandExt;
    command.exec()
}

#[cfg(test)]
mod tests {
    use super::exec;
    use crate::foreground::{output, spawn, status};
    use std::io;
    use crate::foreground::tests::fixture_command;

    #[test]
    fn spawn_transfers_child_and_preserves_configured_output() {
        use std::io::{Read, Seek};
        use std::process::Stdio;
        use std::time::{Duration, Instant};
        let mut destination = tempfile::tempfile().expect("output file");
        let mut command = fixture_command("/bin/sh");
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

    #[test]
    fn exec_returns_native_missing_image_error() {
        let directory = tempfile::tempdir().expect("private fixture directory");
        let mut command = fixture_command(directory.path().join("absent-executable"));
        // No stdio/cwd/env/hook changes: the failed exec leaves this test's
        // process configuration untouched, and never runs another image.
        let error = exec(&mut command);
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
    }

    #[test]
    fn output_preserves_cwd_environment_literal_argv_and_native_nonzero() {
        let directory = tempfile::tempdir().expect("fixture cwd");
        let cwd = directory.path().canonicalize().expect("physical cwd");
        let mut command = fixture_command("/bin/sh");
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

    #[test]
    fn both_execution_forms_preserve_explicit_stdio_overrides() {
        use std::io::{Read, Seek, Write};
        use std::process::Stdio;
        for capture in [false, true] {
            let mut input = tempfile::tempfile().unwrap();
            input.write_all(b"caller-controlled input").unwrap();
            input.rewind().unwrap();
            let mut destination = tempfile::tempfile().unwrap();
            let mut command = fixture_command("/bin/sh");
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
}
