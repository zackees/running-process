//! Owner-private directory permissions without local-IPC transport selection.
//!
//! Persisted registration records use the same host policy as IPC roots, but
//! do not need a listener, endpoint naming, or transport implementation.

/// Result of enforcing owner-private permissions on a local directory.
#[cfg(feature = "private-dir")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerPrivateDirectoryOutcome {
    /// The existing directory already had the complete host policy.
    AlreadyPrivate,
    /// Permissions were applied or repaired.
    Hardened,
}

/// Create a directory and enforce the selected host's owner-private policy.
#[cfg(feature = "private-dir")]
pub fn ensure_owner_private_directory(
    path: &std::path::Path,
) -> std::io::Result<OwnerPrivateDirectoryOutcome> {
    crate::private_dir_ensure_owner_private_directory(path)
}

/// Return whether a directory has the selected host's owner-private policy.
#[cfg(feature = "private-dir")]
pub fn owner_private_directory(path: &std::path::Path) -> std::io::Result<bool> {
    crate::private_dir_owner_private_directory(path)
}
