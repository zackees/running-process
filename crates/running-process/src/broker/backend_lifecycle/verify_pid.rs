//! Process identity verification for backend handles.

use std::io;
use std::path::PathBuf;

use crate::broker::backend_lifecycle::identity::{self, DaemonProcess};
use crate::broker::host_identity;
use crate::platform::process::{self, ProcessInspectError, ProcessInspectErrorKind};

/// Verify a daemon process identity and return an OS liveness handle.
pub fn verify_daemon_process(expected: &DaemonProcess) -> Result<ProcessHandle, VerifyPidError> {
    if expected.pid == 0 {
        return Err(VerifyPidError::InvalidPid(expected.pid));
    }

    let current_boot_id = host_identity::current().boot_id;
    if !expected.boot_id.is_empty()
        && !current_boot_id.is_empty()
        && expected.boot_id != current_boot_id
    {
        return Err(VerifyPidError::BootIdMismatch {
            expected: expected.boot_id.clone(),
            actual: current_boot_id,
        });
    }

    let handle = open_handle(expected.pid)?;
    let exe_path =
        process::executable_path(expected.pid).map_err(|source| VerifyPidError::ExePath {
            pid: expected.pid,
            source,
        })?;
    if !process::same_executable_path(&exe_path, &expected.exe_path) {
        return Err(VerifyPidError::ExePathMismatch {
            pid: expected.pid,
            expected: expected.exe_path.clone(),
            actual: exe_path,
        });
    }

    let actual_hash =
        identity::executable_hash_file(&exe_path).map_err(|source| VerifyPidError::ExeHash {
            pid: expected.pid,
            path: exe_path.clone(),
            source,
        })?;
    if actual_hash != expected.exe_hash {
        return Err(VerifyPidError::ExecutableHashMismatch { pid: expected.pid });
    }

    Ok(handle)
}

/// Return whether a process ID currently resolves to a live process.
pub fn process_is_alive(pid: u32) -> bool {
    ProcessHandle::open(pid).is_ok_and(|handle| handle.is_alive())
}

/// Send a graceful terminate signal where the platform has one.
pub fn signal_terminate(pid: u32) -> Result<(), VerifyPidError> {
    process::signal_terminate(pid).map_err(|error| translate(pid, error))
}

/// Force-kill a process ID.
pub fn force_kill_pid(pid: u32) -> Result<(), VerifyPidError> {
    process::force_kill(pid).map_err(|error| translate(pid, error))
}

/// Errors returned while verifying a daemon process.
#[derive(Debug, thiserror::Error)]
pub enum VerifyPidError {
    /// PID zero or a value outside the native PID range is never valid.
    #[error("invalid daemon pid: {0}")]
    InvalidPid(u32),
    /// The process is not currently alive.
    #[error("process not found: {pid}")]
    NotFound {
        /// Process ID that could not be opened.
        pid: u32,
    },
    /// The manifest was written during a prior host boot.
    #[error("daemon boot id mismatch: expected {expected}, current {actual}")]
    BootIdMismatch {
        /// Boot ID stored with the daemon identity.
        expected: String,
        /// Current host boot ID.
        actual: String,
    },
    /// The executable could not be hashed.
    #[error("failed to hash executable for pid {pid} at {path:?}: {source}")]
    ExeHash {
        /// Process ID being verified.
        pid: u32,
        /// Executable path selected for hashing.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },
    /// The executable path for the process could not be read.
    #[error("failed to resolve executable path for pid {pid}: {source}")]
    ExePath {
        /// Process ID being verified.
        pid: u32,
        /// Underlying platform error.
        source: io::Error,
    },
    /// The executable path did not match the manifest identity.
    #[error(
        "daemon executable path mismatch for pid {pid}: expected {expected:?}, actual {actual:?}"
    )]
    ExePathMismatch {
        /// Process ID being verified.
        pid: u32,
        /// Executable path stored with the daemon identity.
        expected: PathBuf,
        /// Executable path reported by the operating system.
        actual: PathBuf,
    },
    /// The executable hash did not match the manifest identity.
    #[error("daemon executable blake3 hash mismatch for pid {pid}")]
    ExecutableHashMismatch {
        /// Process ID being verified.
        pid: u32,
    },
    /// A platform process-handle operation failed.
    #[error("process handle operation failed for pid {pid}: {source}")]
    Handle {
        /// Process ID being opened or signalled.
        pid: u32,
        /// Underlying platform error.
        source: io::Error,
    },
    /// The platform has no graceful shutdown primitive in this foundation.
    #[error("graceful terminate is unsupported on this platform")]
    GracefulTerminateUnsupported,
}

/// The OS liveness handle this module hands out.
///
/// It is the facade's handle: the ownership rules that make it trustworthy --
/// a pidfd, a kqueue subscription, an open process handle -- belong to the
/// host that issued it, not to this module's vocabulary.
pub use crate::platform::process::ProcessLiveness as ProcessHandle;

fn open_handle(pid: u32) -> Result<ProcessHandle, VerifyPidError> {
    ProcessHandle::open(pid).map_err(|error| translate(pid, error))
}

/// Say a host's answer in this module's vocabulary.
///
/// The three named kinds each have a variant here that predates the facade
/// and that callers already match on. `Host` has no such variant because it
/// is not a classification -- it is the host's own error, and it is carried
/// through whole rather than being given a name it does not have.
fn translate(pid: u32, error: ProcessInspectError) -> VerifyPidError {
    match error.kind {
        ProcessInspectErrorKind::InvalidPid => VerifyPidError::InvalidPid(pid),
        ProcessInspectErrorKind::NotFound => VerifyPidError::NotFound { pid },
        ProcessInspectErrorKind::Unsupported => VerifyPidError::GracefulTerminateUnsupported,
        ProcessInspectErrorKind::Host => VerifyPidError::Handle {
            pid,
            source: error.source,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind a host can report maps onto a variant callers already match.
    ///
    /// Walking the facade's own kinds rather than a local restatement of them
    /// means a new kind stops this compiling until someone decides what this
    /// module should call it.
    #[test]
    fn every_host_kind_has_a_name_here() {
        let staged = |kind| ProcessInspectError {
            kind,
            source: io::Error::from_raw_os_error(1),
        };
        assert!(matches!(
            translate(7, staged(ProcessInspectErrorKind::InvalidPid)),
            VerifyPidError::InvalidPid(7)
        ));
        assert!(matches!(
            translate(7, staged(ProcessInspectErrorKind::NotFound)),
            VerifyPidError::NotFound { pid: 7 }
        ));
        assert!(matches!(
            translate(7, staged(ProcessInspectErrorKind::Unsupported)),
            VerifyPidError::GracefulTerminateUnsupported
        ));
        assert!(matches!(
            translate(7, staged(ProcessInspectErrorKind::Host)),
            VerifyPidError::Handle { pid: 7, .. }
        ));
    }

    /// The host's error survives translation.
    ///
    /// `Handle` exists so an operator can read what the kernel actually said;
    /// replacing it with a message composed here would defeat that.
    #[test]
    fn a_host_error_is_carried_through_whole() {
        let error = translate(
            7,
            ProcessInspectError {
                kind: ProcessInspectErrorKind::Host,
                source: io::Error::from_raw_os_error(13),
            },
        );
        let VerifyPidError::Handle { source, .. } = error else {
            panic!("expected a handle error");
        };
        assert_eq!(source.raw_os_error(), Some(13));
    }

    /// This process is alive; a PID that names nothing is not.
    #[test]
    fn liveness_answers_for_this_process() {
        assert!(process_is_alive(std::process::id()));
        assert!(!process_is_alive(0));
    }
}
