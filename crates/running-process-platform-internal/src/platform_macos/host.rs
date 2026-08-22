//! macOS host facts, directories, user identity, resources, and autostart.

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


/// A stable identity for this user on this machine.
///
/// The uid alone is not enough -- two machines both have a uid 501 -- so it is
/// paired with the platform UUID. Callers hash this; they do not parse it.
pub fn user_machine_identity() -> io::Result<String> {
    let uid = unsafe { libc::getuid() };
    let uuid = platform_uuid()?;
    Ok(format!("{uid}:{uuid}"))
}

/// This machine's `IOPlatformUUID`.
///
/// `ioreg -d2 -c IOPlatformExpertDevice` prints a block containing a line like
/// `"IOPlatformUUID" = "ABCDEF..."`. Parsing that one line is cheaper and more
/// predictable than a full plist parser for a value of this shape.
///
/// This is a fixed-argument, read-only system query with no caller input. It
/// deliberately does not route through the sanitized spawn layer: it runs
/// before any broker endpoint is bound, because the identity it produces is an
/// *input* to the endpoint name.
fn platform_uuid() -> io::Result<String> {
    let output = std::process::Command::new("ioreg")
        .args(["-d2", "-c", "IOPlatformExpertDevice"])
        .output()
        .map_err(|e| io::Error::other(format!("spawn ioreg: {e}")))?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "ioreg failed (status={:?})",
            output.status.code()
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("\"IOPlatformUUID\"") {
            // rest looks like ` = "ABCDEF-..."`
            if let Some(eq_idx) = rest.find('=') {
                let value = rest[eq_idx + 1..].trim();
                let unquoted = value.trim_matches('"');
                if !unquoted.is_empty() {
                    return Ok(unquoted.to_string());
                }
            }
        }
    }
    Err(io::Error::other(
        "ioreg output did not contain IOPlatformUUID",
    ))
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
