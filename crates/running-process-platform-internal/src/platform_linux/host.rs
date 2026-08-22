//! Linux host facts, directories, user identity, resources, and autostart.

use std::io;

/// A privileged system identity this process may be running as.
///
/// The variants name the *answer*, not the mechanism: what a caller does with
/// "this process is the machine's system account" does not change with how the
/// host was asked. `None` from [`current_process_privilege`] means an ordinary
/// user, which is the only case most callers care to distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegedIdentity {
    /// Unix effective UID 0.
    UnixRoot,
    /// Windows LocalSystem account (`S-1-5-18`).
    WindowsLocalSystem,
}

impl std::fmt::Display for PrivilegedIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnixRoot => f.write_str("root (effective uid 0)"),
            Self::WindowsLocalSystem => f.write_str("Windows LocalSystem (S-1-5-18)"),
        }
    }
}

/// The privileged identity this process is running as, if any.
pub fn current_process_privilege() -> io::Result<Option<PrivilegedIdentity>> {
    Ok(privilege_from_effective_uid(unsafe { libc::geteuid() }))
}

/// Root is effective uid 0, and an ordinary uid is not root.
///
/// Kept as a pure function so the rule stays testable without being able to
/// change this process's identity.
fn privilege_from_effective_uid(euid: libc::uid_t) -> Option<PrivilegedIdentity> {
    (euid == 0).then_some(PrivilegedIdentity::UnixRoot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_detection_uses_effective_uid_zero() {
        assert_eq!(
            privilege_from_effective_uid(0),
            Some(PrivilegedIdentity::UnixRoot)
        );
        assert_eq!(privilege_from_effective_uid(1000), None);
    }
}
