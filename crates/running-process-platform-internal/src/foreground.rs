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

// Host-specific forms (Unix `exec`) come from the selected platform tree.
// The Windows tree exports none, so the glob is empty there by design.
#[allow(unused_imports)]
pub use crate::foreground_imp::*;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a fixed fixture command for the per-host foreground tests. The
    /// platform trees reuse this so every fixture `Command` is constructed in
    /// this reviewed escape-hatch module, not a new spawn site.
    pub(crate) fn fixture_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
        Command::new(program)
    }

    #[test]
    fn spawn_preserves_native_missing_program_error() {
        let directory = tempfile::tempdir().expect("private fixture directory");
        let mut command = Command::new(directory.path().join("absent-executable"));
        let error = spawn(&mut command).expect_err("no child should be created");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
