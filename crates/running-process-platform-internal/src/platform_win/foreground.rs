//! Windows foreground execution.
//!
//! Windows has no in-place process-image replacement, so this tree exports no
//! `exec`; the neutral facade's spawn/status/output cover the host.

#[cfg(test)]
mod tests {
    use crate::foreground::output;
    use crate::foreground::tests::fixture_command;

    #[test]
    fn output_preserves_windows_environment_cwd_and_nonzero_status() {
        let directory = tempfile::tempdir().expect("fixture cwd");
        let mut command = fixture_command("cmd.exe");
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
}
