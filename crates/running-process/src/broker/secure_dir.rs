//! Private-directory policy for broker-owned local IPC state.
//!
//! The broker decides which state roots require owner-only access; the
//! selected platform implementation owns permission and ACL mechanics.

use std::io;
use std::path::Path;

/// Create `path` and restrict it to the current user.
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    crate::platform::private_dir::ensure_owner_private_directory(path).map(|_| ())
}

/// Return true when `path` has the selected host's owner-private policy.
pub fn private_dir_permissions_are_private(path: &Path) -> io::Result<bool> {
    crate::platform::private_dir::owner_private_directory(path)
}
