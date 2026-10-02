//! macOS Actually Portable Executable launch mechanics.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

/// An APE image is not a native macOS executable.
pub const APE_NEEDS_LOADER: bool = true;

/// Shell the kernel's `ENOEXEC` convention hands an unrecognized image to.
pub const APE_SHELL: &str = "/bin/sh";

/// Where Cosmopolitan's install instructions place a system-wide loader.
pub const APE_SYSTEM_LOADERS: &[&str] = &["/usr/local/bin/ape"];

/// The prologue's macOS branch compiles its loader with `cc` or patches the
/// extracted image; neither is a loader this crate can run as extracted.
pub const APE_EMBEDDED_LOADER: bool = false;

/// Whether this libc's `execvp` runs an `ENOEXEC` image with the shell, as
/// POSIX requires. Apple's libc does.
pub const APE_EXECVP_SHELL_FALLBACK: bool = true;

/// The kernel refused the image's format.
pub fn is_exec_format_error(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENOEXEC)
}

/// Whether metadata carries any execute permission bit.
pub fn is_executable(metadata: &std::fs::Metadata) -> bool {
    metadata.permissions().mode() & 0o111 != 0
}

/// No extracted loader is installed on macOS ([`APE_EMBEDDED_LOADER`]).
pub fn default_loader_dirs() -> Vec<PathBuf> {
    Vec::new()
}

/// No extracted loader is installed on macOS ([`APE_EMBEDDED_LOADER`]).
pub fn materialize_loader(_bytes: &[u8], _name: &str, _dirs: &[PathBuf]) -> Option<PathBuf> {
    None
}

/// Make an extracted loader executable by its owner and readable by others.
pub fn mark_executable(path: &Path) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

/// Make the next spawn fork and `execvp` instead of `posix_spawn`.
///
/// `posix_spawn` reports `ENOEXEC` as is; `execvp` retries the image with
/// `/bin/sh`. Any `pre_exec` hook makes std take the second path, and this
/// one does nothing else.
pub fn route_through_execvp(command: &mut std::process::Command) {
    // SAFETY: the hook touches no state at all, so it is async-signal-safe.
    unsafe {
        command.pre_exec(|| Ok(()));
    }
}

/// [`route_through_execvp`] for a Tokio command.
#[cfg(feature = "async-process")]
pub fn route_tokio_through_execvp(command: &mut tokio::process::Command) {
    // SAFETY: the hook touches no state at all, so it is async-signal-safe.
    unsafe {
        command.pre_exec(|| Ok(()));
    }
}

#[cfg(test)]
#[path = "ape_tests.rs"]
mod tests;
