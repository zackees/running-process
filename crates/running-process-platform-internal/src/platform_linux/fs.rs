//! Linux per-user directory placement for product runtime artifacts.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// Directory for `product`'s ephemeral runtime artifacts (sockets, pid files).
///
/// `XDG_RUNTIME_DIR` is the session-scoped, per-user location the platform
/// provides for exactly this. Where it is absent the fallback qualifies the
/// name with the caller's uid, because `/tmp` is shared and two accounts must
/// not land on one directory.
pub fn user_runtime_dir(product: &str) -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join(product);
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/{product}-{uid}"))
}

/// Directory for `product`'s persistent state (databases that outlive a boot).
pub fn user_state_dir(product: &str) -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(dir).join(product)
    } else if let Some(home) = dirs::home_dir() {
        home.join(".local/state").join(product)
    } else {
        PathBuf::from(format!("/tmp/{product}-state"))
    }
}

/// Root under which `product` keeps per-run scratch data.
///
/// Linux has no separate location for this: run data is as ephemeral as the
/// runtime artifacts and belongs beside them.
pub fn user_run_data_root(product: &str) -> PathBuf {
    user_runtime_dir(product)
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
