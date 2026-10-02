//! linux Actually Portable Executable launch mechanics.
//!
//! A loader extracted from an APE image is installed content-addressed into
//! the first candidate directory that is owned by the effective user, not
//! group/world-writable, and on a mount without `noexec`. When none
//! qualifies (read-only home, `noexec` `/tmp`, no runtime dir) the loader is
//! placed in a sealed `memfd` held open for the life of the process and
//! exec'd via `/proc/self/fd/N`.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// An APE image is not a native Linux executable.
pub const APE_NEEDS_LOADER: bool = true;

/// Shell the kernel's `ENOEXEC` convention hands an unrecognized image to.
pub const APE_SHELL: &str = "/bin/sh";

/// Where Cosmopolitan's install instructions place a system-wide loader.
pub const APE_SYSTEM_LOADERS: &[&str] = &["/usr/bin/ape", "/usr/local/bin/ape"];

/// The prologue's Linux branch carries a loader that runs as extracted.
pub const APE_EMBEDDED_LOADER: bool = true;

/// Whether this libc's `execvp` runs an `ENOEXEC` image with the shell, as
/// POSIX requires. glibc does; musl does not.
#[cfg(target_env = "musl")]
pub const APE_EXECVP_SHELL_FALLBACK: bool = false;
/// Whether this libc's `execvp` runs an `ENOEXEC` image with the shell, as
/// POSIX requires. glibc does; musl does not.
#[cfg(not(target_env = "musl"))]
pub const APE_EXECVP_SHELL_FALLBACK: bool = true;

/// The kernel refused the image's format.
pub fn is_exec_format_error(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENOEXEC)
}

/// Whether metadata carries any execute permission bit.
pub fn is_executable(metadata: &std::fs::Metadata) -> bool {
    metadata.permissions().mode() & 0o111 != 0
}

/// Make an extracted loader executable by its owner and readable by others.
pub fn mark_executable(path: &Path) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

/// Make the next spawn fork and `execvp` instead of `posix_spawn`.
///
/// glibc's `posix_spawn` reports `ENOEXEC` as is; its `execvp` retries the
/// image with `/bin/sh`. Any `pre_exec` hook makes std take the second path,
/// and this one does nothing else.
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

/// Default directories for extracted loaders, most durable first.
pub fn default_loader_dirs() -> Vec<PathBuf> {
    let set = |value: Option<std::ffi::OsString>| value.filter(|value| !value.is_empty());
    let mut dirs = Vec::new();
    let cache = set(crate::env_vars::XDG_CACHE_HOME.os())
        .map(PathBuf::from)
        .or_else(|| set(crate::env_vars::HOME.os()).map(|home| PathBuf::from(home).join(".cache")));
    if let Some(cache) = cache {
        dirs.push(cache.join("running-process").join("ape"));
    }
    if let Some(runtime) = set(crate::env_vars::XDG_RUNTIME_DIR.os()) {
        dirs.push(PathBuf::from(runtime).join("running-process").join("ape"));
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    dirs.push(std::env::temp_dir().join(format!("running-process-ape-{uid}")));
    dirs
}

/// Install `bytes` as an executable named `name` and return its exec path.
pub fn materialize_loader(bytes: &[u8], name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .find_map(|dir| install_in(dir, bytes, name))
        .or_else(|| memfd_loader(bytes, name))
}

fn install_in(dir: &Path, bytes: &[u8], name: &str) -> Option<PathBuf> {
    if !private_dir(dir) || !mount_allows_exec(dir) {
        return None;
    }
    let target = dir.join(name);
    if is_installed(&target, bytes) {
        return Some(target);
    }
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let staging = dir.join(format!(
        ".{name}.{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    // No child may inherit the writable descriptor, or executing the loader
    // would hit ETXTBSY until that child execs.
    let written = {
        let _fork = crate::platform::ape::exclusive_fork_guard();
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&staging)
            .and_then(|mut file| {
                file.write_all(bytes)?;
                file.sync_all()
            })
    }
    // rename(2) is atomic: concurrent installers race benignly to identical
    // content, and readers never see a partial file.
    .and_then(|()| std::fs::rename(&staging, &target));
    if written.is_err() {
        let _ = std::fs::remove_file(&staging);
        return None;
    }
    is_installed(&target, bytes).then_some(target)
}

/// A regular, owner-executable file whose content is exactly `bytes`.
fn is_installed(path: &Path, bytes: &[u8]) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| {
        meta.file_type().is_file()
            && meta.permissions().mode() & 0o100 != 0
            && meta.len() == bytes.len() as u64
    }) && std::fs::read(path).is_ok_and(|content| content == bytes)
}

/// Create `dir` (mode 0700) if needed and accept it only when it is a real
/// directory owned by the effective user with no group/other write access,
/// so no other account can plant or swap a loader in it.
fn private_dir(dir: &Path) -> bool {
    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir);
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    std::fs::symlink_metadata(dir).is_ok_and(|meta| {
        meta.file_type().is_dir() && meta.uid() == uid && meta.mode() & 0o022 == 0
    })
}

fn mount_allows_exec(dir: &Path) -> bool {
    let Ok(path) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stats` points to writable storage.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return false;
    }
    // SAFETY: a successful statvfs initialized the complete output structure.
    let stats = unsafe { stats.assume_init() };
    stats.f_flag & libc::ST_NOEXEC == 0
}

/// Sealed memfds kept open for the process lifetime, keyed by loader name.
static MEMFDS: Mutex<Option<HashMap<String, File>>> = Mutex::new(None);

fn memfd_loader(bytes: &[u8], name: &str) -> Option<PathBuf> {
    let mut guard = MEMFDS.lock().unwrap_or_else(|error| error.into_inner());
    let fds = guard.get_or_insert_with(HashMap::new);
    if let Some(file) = fds.get(name) {
        return Some(fd_path(file));
    }
    let file = {
        // The memfd is writable until sealed; keep it out of forked children.
        let _fork = crate::platform::ape::exclusive_fork_guard();
        create_sealed_memfd(bytes, name)?
    };
    let path = fd_path(&file);
    // The child resolves this path before close-on-exec runs; without /proc
    // it cannot, so refuse rather than hand out a dead path.
    if !path.exists() {
        return None;
    }
    fds.insert(name.to_owned(), file);
    Some(path)
}

fn create_sealed_memfd(bytes: &[u8], name: &str) -> Option<File> {
    let cname = CString::new(name).ok()?;
    let base = libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING;
    // MFD_EXEC (Linux 6.3+) keeps the memfd executable under
    // `vm.memfd_noexec=1`; older kernels reject the flag with EINVAL.
    let fd = [base | libc::MFD_EXEC, base].into_iter().find_map(|flags| {
        // SAFETY: `cname` is NUL-terminated; the flags are valid memfd flags.
        let fd = unsafe { libc::memfd_create(cname.as_ptr(), flags) };
        (fd >= 0).then_some(fd)
    })?;
    // SAFETY: `fd` is a freshly created descriptor owned by nobody else.
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes).ok()?;
    file.set_permissions(std::fs::Permissions::from_mode(0o500))
        .ok()?;
    let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL;
    // SAFETY: `file` owns a valid memfd created with MFD_ALLOW_SEALING.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } != 0 {
        return None;
    }
    Some(file)
}

fn fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

#[cfg(test)]
#[path = "ape_tests.rs"]
mod tests;
