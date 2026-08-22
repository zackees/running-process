//! Per-OS boot autostart for the `runpm` daemon (Phase 4 of #222 — #427).
//!
//! What this module decides is *enrolment*: that the thing being registered is
//! the runpm daemon, what it is called on each host, and that it starts with
//! `start` and stops with `stop`. How a host registers a program to run at
//! login — systemd user unit, launchd agent, Task Scheduler ONLOGON task — is
//! [`crate::platform::autostart`]'s business, and the names below are the only
//! part of it that is ours.
//!
//! The trio is unchanged:
//!   - `install(daemon_binary)` — write the unit/plist/task and arm the init
//!     system. Returns the unit path that was written.
//!   - `uninstall()` — disarm the init system and remove the unit.
//!   - `render_unit(daemon_binary)` — render the unit text without touching
//!     the filesystem. Used by fixture tests and by `install`.
//!
//! Tests never call `install` — they assert against `render_unit` output to
//! avoid mutating the runner's init system.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::platform::autostart::{AutostartError, AutostartProgram};

/// Plain stem: the systemd unit name and the Task Scheduler task name.
const IDENTIFIER: &str = "runpm-daemon";

/// Reverse-DNS label, as launchd requires.
const LABEL: &str = "com.zackees.runpm-daemon";

/// What an operator reading their init system should see.
const DESCRIPTION: &str = "runpm process supervisor (running-process daemon)";

/// Typed wrapper around the path where the unit/plist/task was written.
/// Wrapped so callers can't accidentally pass it as a generic `PathBuf`
/// and lose the "this is the autostart artifact" intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitPath(pub PathBuf);

impl UnitPath {
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_inner(self) -> PathBuf {
        self.0
    }
}

impl fmt::Display for UnitPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

/// Anything that can go wrong installing/uninstalling boot autostart.
#[derive(Debug)]
pub enum BootAutostartError {
    /// Could not resolve where to write the unit file.
    Resolve(String),
    /// Filesystem write/remove failed.
    Io(std::io::Error),
    /// The init-system CLI (`systemctl`, `launchctl`, `schtasks`) failed.
    InitSystem(String),
    /// This OS has no autostart backend.
    ///
    /// Retained for compatibility. It is no longer produced: the platform
    /// facade is built for exactly the hosts this crate compiles on, so a host
    /// without a backend fails to build rather than failing at runtime.
    Unsupported(String),
}

impl fmt::Display for BootAutostartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolve(detail) => write!(f, "could not resolve autostart location: {detail}"),
            Self::Io(error) => write!(f, "autostart file operation failed: {error}"),
            Self::InitSystem(detail) => write!(f, "init system rejected autostart: {detail}"),
            Self::Unsupported(os) => write!(f, "boot autostart is not supported on {os}"),
        }
    }
}

impl std::error::Error for BootAutostartError {}

impl From<std::io::Error> for BootAutostartError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<AutostartError> for BootAutostartError {
    fn from(error: AutostartError) -> Self {
        match error {
            AutostartError::Resolve(detail) => Self::Resolve(detail),
            AutostartError::Io(io) => Self::Io(io),
            AutostartError::InitSystem(detail) => Self::InitSystem(detail),
        }
    }
}

/// The runpm daemon, as this host should know it.
fn runpm_daemon(daemon_binary: &Path) -> AutostartProgram<'_> {
    AutostartProgram {
        identifier: IDENTIFIER,
        label: LABEL,
        description: DESCRIPTION,
        program: daemon_binary,
        start_argument: "start",
        stop_argument: "stop",
    }
}

/// Install boot autostart for the running-process daemon. Returns the
/// path where the unit/plist/task was written.
pub fn install(daemon_binary: &Path) -> Result<UnitPath, BootAutostartError> {
    let written = crate::platform::autostart::register(&runpm_daemon(daemon_binary))?;
    Ok(UnitPath(written))
}

/// Uninstall boot autostart for the running-process daemon.
pub fn uninstall() -> Result<(), BootAutostartError> {
    // The path is not consulted for removal, but the names are, so the same
    // description is handed over to keep one definition of what runpm is.
    Ok(crate::platform::autostart::unregister(&runpm_daemon(
        Path::new(""),
    ))?)
}

/// Render the unit/plist/task text for the current OS without touching
/// the filesystem. Test seam used by `tests/runpm_boot_autostart_fixtures.rs`.
pub fn render_unit(daemon_binary: &Path) -> String {
    crate::platform::autostart::render_registration(&runpm_daemon(daemon_binary))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever this host writes, it is identifiably runpm's, and it names the
    /// binary it was handed.
    ///
    /// Which of the three names appears is the host's choice, not ours: the
    /// systemd unit carries the identifier in its *filename* and only the
    /// description in its body, launchd puts the reverse-DNS label in the
    /// plist, and Task Scheduler puts the plain stem in `/TN`. So this asserts
    /// what is true of all three -- an operator reading the registration can
    /// tell whose it is -- rather than picking one host's spelling.
    #[test]
    fn the_rendered_registration_is_for_the_runpm_daemon() {
        let rendered = render_unit(Path::new("/usr/local/bin/running-process-daemon"));
        assert!(
            rendered.contains("running-process-daemon"),
            "must name the daemon binary: {rendered}"
        );
        assert!(
            rendered.contains("runpm"),
            "an operator must be able to tell whose registration this is: {rendered}"
        );
    }

    /// Every facade failure maps onto the variant an operator acts on, so the
    /// distinction survives the hop across the boundary.
    #[test]
    fn facade_errors_keep_their_kind() {
        assert!(matches!(
            BootAutostartError::from(AutostartError::Resolve("no HOME".into())),
            BootAutostartError::Resolve(_)
        ));
        assert!(matches!(
            BootAutostartError::from(AutostartError::InitSystem("schtasks".into())),
            BootAutostartError::InitSystem(_)
        ));
        assert!(matches!(
            BootAutostartError::from(AutostartError::Io(std::io::Error::other("disk"))),
            BootAutostartError::Io(_)
        ));
    }
}
