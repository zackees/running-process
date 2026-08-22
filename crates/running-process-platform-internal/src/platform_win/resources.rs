//! What Windows says about running out of things.

use std::io;
use std::path::Path;

use crate::platform::resources::InodeCapacity;

/// WSAEMFILE -- a socket call that could not get a descriptor.
const WSAEMFILE: i32 = 10024;
/// ERROR_TOO_MANY_OPEN_FILES.
const ERROR_TOO_MANY_OPEN_FILES: i32 = 4;
/// ERROR_NO_SYSTEM_RESOURCES -- the system-wide form of the same wall.
const ERROR_NO_SYSTEM_RESOURCES: i32 = 1450;

/// ERROR_HANDLE_DISK_FULL.
const ERROR_HANDLE_DISK_FULL: i32 = 39;
/// ERROR_DISK_FULL.
const ERROR_DISK_FULL: i32 = 112;

/// Whether this error means the process or the system is out of descriptors.
pub fn signals_fd_exhaustion(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(WSAEMFILE | ERROR_TOO_MANY_OPEN_FILES | ERROR_NO_SYSTEM_RESOURCES)
    )
}

/// Whether this error means the filesystem is out of space.
pub fn signals_storage_exhaustion(error: &io::Error) -> bool {
    if matches!(error.kind(), io::ErrorKind::StorageFull) {
        return true;
    }
    matches!(
        error.raw_os_error(),
        Some(ERROR_HANDLE_DISK_FULL | ERROR_DISK_FULL)
    )
}

/// One error this host would report for descriptor exhaustion.
pub fn fd_exhaustion_error() -> io::Error {
    io::Error::from_raw_os_error(WSAEMFILE)
}

/// One error this host would report for storage exhaustion.
pub fn storage_exhaustion_error() -> io::Error {
    io::Error::from_raw_os_error(ERROR_DISK_FULL)
}

/// Windows filesystems have no fixed inode table, so there is nothing to
/// report -- and reporting invented numbers would be worse than saying so.
pub fn inode_capacity(path: &Path) -> io::Result<Option<InodeCapacity>> {
    let _ = path;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not applicable is the answer, and it must stay an answer rather than
    /// becoming an error or an invented zero-of-zero: a caller presenting
    /// inode pressure has to be able to tell "this host does not have that"
    /// from "the probe failed".
    #[test]
    fn inode_usage_is_not_applicable_on_windows() {
        let probed =
            inode_capacity(&std::env::temp_dir()).expect("the probe never fails on Windows");
        assert_eq!(probed, None);
    }
}
