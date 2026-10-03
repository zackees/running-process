//! macOS Actually Portable Executable launch mechanics: the default loader
//! cache directories and the private-directory check installs rely on.
//! macOS has no anonymous-executable fallback (no memfd), so a usable cache
//! directory is required; `$TMPDIR` is already per-user there.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

/// An APE image is not a native macOS executable.
pub const APE_NEEDS_LOADER: bool = true;

/// Shell the kernel's `ENOEXEC` convention hands an unrecognized image to.
pub const APE_SHELL: &str = "/bin/sh";

/// Where Cosmopolitan's install instructions place a system-wide loader.
pub const APE_SYSTEM_LOADERS: &[&str] = &["/usr/local/bin/ape"];

/// Which embedded loader this host runs: the x86_64 Mach-O loader, or the
/// Apple Silicon loader compiled from the image's `ape-m1.c`.
pub const APE_LOADER_HOST: crate::platform::ape::LoaderHost = crate::platform::ape::LoaderHost::Macos;

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

/// Default directories for installed loaders, most durable first.
pub fn default_loader_dirs() -> Vec<PathBuf> {
    let set = |value: Option<std::ffi::OsString>| value.filter(|value| !value.is_empty());
    let mut dirs = Vec::new();
    if let Some(cache) = set(crate::env_vars::XDG_CACHE_HOME.os()) {
        dirs.push(PathBuf::from(cache).join("running-process").join("ape"));
    }
    if let Some(home) = set(crate::env_vars::HOME.os()) {
        dirs.push(
            PathBuf::from(home)
                .join("Library")
                .join("Caches")
                .join("running-process")
                .join("ape"),
        );
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    dirs.push(std::env::temp_dir().join(format!("running-process-ape-{uid}")));
    dirs
}

/// Whether `dir` may hold an executable this crate installs: create it (mode
/// 0700) if needed, then accept it only when it is a real directory owned by
/// the effective user with no group/other write access, on a mount that
/// allows exec.
pub fn private_exec_dir(dir: &Path) -> bool {
    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir);
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let private = std::fs::symlink_metadata(dir).is_ok_and(|meta| {
        meta.file_type().is_dir() && meta.uid() == uid && meta.mode() & 0o022 == 0
    });
    private && mount_allows_exec(dir)
}

fn mount_allows_exec(dir: &Path) -> bool {
    let Ok(path) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stats` points to writable storage.
    if unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return false;
    }
    // SAFETY: a successful statfs initialized the complete output structure.
    let stats = unsafe { stats.assume_init() };
    stats.f_flags & (libc::MNT_NOEXEC as u32) == 0
}

/// macOS has no memfd; an installed loader needs a usable cache directory.
pub fn anonymous_executable(_bytes: &[u8], _name: &str) -> Option<PathBuf> {
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
