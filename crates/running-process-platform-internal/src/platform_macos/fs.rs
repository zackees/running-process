//! macOS per-user directory placement for product runtime artifacts.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// Directory for `product`'s ephemeral runtime artifacts (sockets, pid files).
///
/// macOS ships no `XDG_RUNTIME_DIR`, but honours it when a caller's
/// environment sets one. The fallback qualifies `/tmp` with the caller's uid,
/// because `/tmp` is shared and two accounts must not land on one directory.
pub fn user_runtime_dir(product: &str) -> PathBuf {
    if let Some(dir) = crate::env_vars::XDG_RUNTIME_DIR.os() {
        return PathBuf::from(dir).join(product);
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/{product}-{uid}"))
}

/// Directory for `product`'s persistent state (databases that outlive a boot).
pub fn user_state_dir(product: &str) -> PathBuf {
    if let Some(dir) = crate::env_vars::XDG_STATE_HOME.os() {
        PathBuf::from(dir).join(product)
    } else if let Some(home) = dirs::home_dir() {
        home.join(".local/state").join(product)
    } else {
        PathBuf::from(format!("/tmp/{product}-state"))
    }
}

/// Directory for `product`'s persistent state, derived from the environment
/// alone: `XDG_STATE_HOME`, then `$HOME/.local/state`, then
/// `/tmp/{product}-state`.
///
/// Unlike [`user_state_dir`] this never consults the account database, so the
/// location is exactly what the environment says. That is the documented
/// contract of the probe worker's symbol cache (#974), and a caller that
/// publishes such a contract needs a primitive that keeps it.
pub fn user_state_dir_from_environment(product: &str) -> PathBuf {
    state_dir_from_environment_in(
        crate::env_vars::XDG_STATE_HOME.os(),
        crate::env_vars::HOME.os(),
        product,
    )
}

/// The base directory for per-user persistent state, derived from the
/// environment alone: `XDG_STATE_HOME`, then `$HOME/.local/state`.
///
/// `None` when neither is set; the caller picks its own fallback and its own
/// leaf beneath the base.
pub fn state_home_from_environment() -> Option<PathBuf> {
    state_home_in(
        crate::env_vars::XDG_STATE_HOME.os(),
        crate::env_vars::HOME.os(),
    )
}

fn state_home_in(
    xdg_state_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    xdg_state_home.map(PathBuf::from).or_else(|| {
        home.map(PathBuf::from)
            .map(|home| home.join(".local").join("state"))
    })
}

fn state_dir_from_environment_in(
    xdg_state_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    product: &str,
) -> PathBuf {
    state_home_in(xdg_state_home, home)
        .map_or_else(
            || PathBuf::from(format!("/tmp/{product}-state")),
            |base| base.join(product),
        )
}

/// Root under which `product` keeps per-run scratch data.
///
/// macOS gives each user a `Library/Caches` for exactly this class of data,
/// which is where it belongs rather than beside the sockets.
pub fn user_run_data_root(product: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Library/Caches")
        .join(product)
}

/// Stable identity of an open file on this host.
///
/// Two paths that resolve to the same bytes on disk report the same identity,
/// which is what lets a caller notice that the file it opened has since been
/// replaced. The two fields are whatever this host uses to say that: a device
/// and inode, a volume serial and file index, or an equivalent pair. Callers
/// compare them; they do not interpret them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    /// Device, volume, or platform-equivalent file namespace.
    pub device: u64,
    /// Inode, file index, or platform-equivalent file number.
    pub file: u64,
}

/// Identity of an already-open file.
pub fn file_identity(file: &File) -> io::Result<Option<FileIdentity>> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = file.metadata()?;
    Ok(Some(FileIdentity {
        device: metadata.dev(),
        file: metadata.ino(),
    }))
}

/// Identity of the file a path currently names.
pub fn path_identity(path: &Path) -> io::Result<Option<FileIdentity>> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = path.metadata()?;
    Ok(Some(FileIdentity {
        device: metadata.dev(),
        file: metadata.ino(),
    }))
}

/// Open `path` for use as an advisory lock file, creating it if absent.
///
/// The mode matters as much as the open: a lock file another account can
/// rewrite is not a lock. Unix answers that with owner-only permissions.
pub fn open_lock_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // Never truncate: an existing lock file may be held right now,
        // and its contents are not ours to clear.
        .truncate(false)
        .mode(0o600)
        .open(path)
}

/// Take an exclusive advisory lock without waiting.
///
/// Returns immediately when another holder has it; the caller decides whether
/// that is a conflict worth retrying, via [`is_lock_conflict`].
pub fn try_lock_exclusive(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd as _;

    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Release a lock taken by [`try_lock_exclusive`].
pub fn unlock(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd as _;

    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Whether `error` means "someone else holds it", rather than a real failure.
///
/// Hosts spell this differently and callers must not have to know which; the
/// distinction decides whether waiting is worthwhile.
pub fn is_lock_conflict(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::EWOULDBLOCK) || error.raw_os_error() == Some(libc::EAGAIN)
}

/// Encode `path` as the bytes this host uses to spell it.
///
/// Faithful, not canonical: this is the encoding a path is carried in so the
/// other end can reconstruct exactly the path that was named. It is
/// deliberately not `ipc::endpoint_scope_bytes`, which folds away differences
/// a host considers meaningless in order to hash two spellings to one identity.
/// Round-tripping through the pair here must return the original path;
/// round-tripping through that one need not.
pub fn encode_path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;

    path.as_os_str().as_bytes().to_vec()
}

/// Reconstruct a path from [`encode_path_bytes`] output produced on this host.
pub fn decode_path_bytes(bytes: &[u8]) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;

    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
}

/// Directory for `product`'s shared application data.
///
/// Distinct from [`user_state_dir`]: state is this machine's private
/// bookkeeping, while this is the data a user expects to follow their account.
/// macOS keeps it under `Application Support`.
pub fn user_data_dir(product: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Library")
        .join("Application Support")
        .join(product)
}

/// The directory this host keeps a product's *configuration* in.
///
/// The same location as [`user_data_dir`] here. macOS does not separate the
/// two, and inventing a split would put files where no macOS user or tool
/// looks for them.
pub fn user_config_dir(product: &str) -> PathBuf {
    user_data_dir(product)
}

/// Move `tmp` onto `target`, replacing it, without a window where neither is
/// readable.
pub fn replace_file(tmp: &Path, target: &Path) -> io::Result<()> {
    std::fs::rename(tmp, target)
}

/// Make a directory entry created by [`replace_file`] durable.
///
/// A rename is only as durable as the directory recording it, which Unix does
/// not flush with the file.
pub fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

/// Create a new file that only its owner can read, failing if it exists.
///
/// `create_new` is part of the contract, not a convenience: a private file
/// opened over one that already exists inherits whatever that one allowed.
/// Unix sets the mode at creation, so there is no window where the file exists
/// with broader permissions.
pub fn create_private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Open `path` for reading without following a link at its final component.
///
/// The kernel refuses a symlink there (`ELOOP`), so the handle is always the
/// named object itself, never a target substituted after the caller
/// validated the name.
pub fn open_read_no_follow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Whether metadata of an [`open_read_no_follow`] handle names a link rather
/// than the object itself. The open already refused links here, so this
/// reads the file type the handle reports.
pub fn is_link_handle(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
mod no_follow_tests {
    #[test]
    fn state_dir_from_environment_prefers_xdg_then_home_then_tmp() {
        use super::state_dir_from_environment_in as dir;
        use std::path::PathBuf;
        assert_eq!(
            dir(Some("/xdg".into()), Some("/home/u".into()), "rp"),
            PathBuf::from("/xdg/rp")
        );
        assert_eq!(
            dir(None, Some("/home/u".into()), "rp"),
            PathBuf::from("/home/u/.local/state/rp")
        );
        assert_eq!(dir(None, None, "rp"), PathBuf::from("/tmp/rp-state"));
    }

    /// #975: the state base without a product leaf or fallback, which the py
    /// tracked-pid registry joins its own leaf onto (temp dir when `None`).
    #[test]
    fn state_home_prefers_xdg_then_home_and_has_no_fallback() {
        use super::state_home_in as home;
        use std::path::PathBuf;
        assert_eq!(
            home(Some("/xdg".into()), Some("/home/u".into())),
            Some(PathBuf::from("/xdg"))
        );
        assert_eq!(
            home(None, Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.local/state"))
        );
        assert_eq!(home(None, None), None);
    }

    /// #974 PR 2: probe-daemon validates an artifact's name, then opens it;
    /// a symlink swapped in between must not redirect the open.
    #[test]
    fn a_no_follow_open_refuses_a_symlink_at_the_final_component() {
        let dir = std::env::temp_dir().join(format!("rp-nofollow-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        let link = dir.join("link");
        std::fs::write(&target, b"secret").unwrap();
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let error = super::open_read_no_follow(&link).expect_err("a symlink must not be followed");
        assert_eq!(error.raw_os_error(), Some(libc::ELOOP));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
