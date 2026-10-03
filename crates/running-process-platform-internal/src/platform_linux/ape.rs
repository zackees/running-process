//! linux Actually Portable Executable launch mechanics: the default loader
//! cache directories, the private-directory check installs rely on, and the
//! sealed `memfd` fallback used when no cache directory is usable (read-only
//! home, `noexec` `/tmp`, no runtime dir). The memfd is held open for the life
//! of the process and exec'd via `/proc/self/fd/N`.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// An APE image is not a native Linux executable.
pub const APE_NEEDS_LOADER: bool = true;

/// Shell the kernel's `ENOEXEC` convention hands an unrecognized image to.
pub const APE_SHELL: &str = "/bin/sh";

/// Where Cosmopolitan's install instructions place a system-wide loader.
pub const APE_SYSTEM_LOADERS: &[&str] = &["/usr/bin/ape", "/usr/local/bin/ape"];

/// Which embedded loader this host runs: the Linux static ELF.
pub const APE_LOADER_HOST: crate::platform::ape::LoaderHost = crate::platform::ape::LoaderHost::Linux;

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

/// Whether `dir` may hold an executable this crate installs: create it (mode
/// 0700) if needed, then accept it only when it is a real directory owned by
/// the effective user with no group/other write access -- so no other account
/// can plant or swap a file in it -- on a mount that allows exec.
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
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stats` points to writable storage.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return false;
    }
    // SAFETY: a successful statvfs initialized the complete output structure.
    let stats = unsafe { stats.assume_init() };
    stats.f_flag & libc::ST_NOEXEC == 0
}

/// Last-resort executable with no filesystem home: a sealed memfd exec'd via
/// `/proc/self/fd/N`. Valid only for direct children of this process.
pub fn anonymous_executable(bytes: &[u8], name: &str) -> Option<PathBuf> {
    memfd_loader(bytes, name)
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
        // Raw syscall, not the libc wrapper: a binary linked against an old
        // glibc (2.17 for manylinux2014 wheels) predates `memfd_create()`,
        // which glibc added in 2.27.
        // SAFETY: `cname` is NUL-terminated; the flags are valid memfd flags.
        let fd = unsafe { libc::syscall(libc::SYS_memfd_create, cname.as_ptr(), flags) };
        i32::try_from(fd).ok().filter(|fd| *fd >= 0)
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
