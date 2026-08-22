//! macOS host facts, directories, user identity, resources, and autostart.

use std::io;
use std::path::Path;

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

// ---------------------------------------------------------------------------
// Host identity facts
// ---------------------------------------------------------------------------

/// This machine's name as the host reports it.
pub fn hostname() -> Option<String> {
    let mut buf = [0_u8; 256];
    // SAFETY: `buf` is writable for its full length, which is what is passed
    // as the bound. The kernel NUL-terminates within it on success.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if ok != 0 {
        return None;
    }
    let nul = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..nul]).into_owned();
    (!name.is_empty()).then_some(name)
}

/// The filesystem device this path lives on.
pub fn filesystem_device_id(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;

    std::fs::metadata(path).ok().map(|meta| meta.dev())
}

/// A durable per-machine identifier that survives reboots.
///
/// The hardware answer is `IOPlatformUUID`, which [`user_machine_identity`]
/// already reads. This fact keeps the hostname-derived form instead: it is
/// recorded verbatim into on-disk manifests, and changing the spelling would
/// make every manifest written by an older build compare as a different
/// machine. Switching the two onto one source is a data-format change, not a
/// refactor, so it is left to whoever is willing to migrate the manifests.
pub fn machine_id() -> Option<String> {
    hostname().map(|name| format!("macos-{name}"))
}

/// An identifier that changes on every boot of this machine.
///
/// macOS has no boot uuid, so the kernel's recorded boot instant stands in for
/// one: it is fixed for the life of a boot and differs across boots.
pub fn boot_id() -> Option<String> {
    boot_time().map(|(seconds, micros)| format!("macos-boot-{seconds}-{micros}"))
}

fn boot_time() -> Option<(i64, i64)> {
    use std::ffi::CString;

    let name = CString::new("kern.boottime").expect("static sysctl name");
    // SAFETY: `boot` is a plain-old-data timeval, and an all-zero bit pattern
    // is a valid one.
    let mut boot: libc::timeval = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::timeval>();
    // SAFETY: the name is a NUL-terminated static string alive for the call,
    // and `boot`/`len` are valid writable storage of exactly the size the
    // kernel is told to write.
    let ok = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut boot as *mut libc::timeval).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    let recorded = (boot.tv_sec as i64, boot.tv_usec as i64);
    (ok == 0).then_some(recorded)
}

/// macOS has no process namespaces of the kind this identity distinguishes.
pub fn namespace_id() -> Option<String> {
    None
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

    /// The three string facts either answer or say they cannot; an empty
    /// string is never a valid answer, because a caller comparing two hosts
    /// would read two empties as a match.
    #[test]
    fn host_identity_facts_are_never_empty_strings() {
        let facts = [hostname(), machine_id(), boot_id(), namespace_id()];
        for value in facts.into_iter().flatten() {
            assert!(!value.is_empty(), "a reported fact must carry a value");
        }
    }

    /// This host has a name and a machine id, whatever they turn out to be.
    #[test]
    fn this_host_reports_a_name_and_a_machine_id() {
        assert!(hostname().is_some(), "a running host has a name");
        assert!(machine_id().is_some(), "a running host has a machine id");
    }

    /// The device id is a property of the volume, so a path and its parent
    /// answer alike, and a directory that exists always answers.
    #[test]
    fn filesystem_device_id_answers_for_an_existing_directory() {
        let cwd = std::env::current_dir().expect("cwd");
        let dev = filesystem_device_id(&cwd).expect("an existing directory has a device");
        assert_eq!(filesystem_device_id(&cwd), Some(dev), "stable across reads");
    }

    /// A path that does not exist has no device to report. Unlike the volume
    /// probe on Windows, there is nothing to walk up to here: the caller asked
    /// about a path, and the honest answer is that the host does not know.
    #[test]
    fn filesystem_device_id_declines_a_missing_path() {
        let missing = std::env::temp_dir().join(format!(
            "rp-host-absent-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        assert_eq!(filesystem_device_id(&missing), None);
    }
}
