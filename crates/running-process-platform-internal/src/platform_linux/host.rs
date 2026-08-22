//! Linux host facts, directories, user identity, resources, and autostart.

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
/// The uid alone is not enough -- two machines both have a uid 1000 -- so it
/// is paired with a machine-scoped id. Callers hash this; they do not parse it.
pub fn user_machine_identity() -> io::Result<String> {
    let uid = unsafe { libc::getuid() };
    let machine_id = crate::platform::host::machine_id_from(&MACHINE_ID_PATHS, BOOT_ID_PATH)?;
    Ok(format!("{uid}:{machine_id}"))
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
/// `/etc/machine-id` is the systemd-era answer and `/var/lib/dbus/machine-id`
/// the older one; a host with neither falls back to a boot-scoped id, which is
/// weaker but still never shared between machines.
///
/// Deliberately *not* [`crate::platform::host::machine_id_from`], which treats
/// an unreadable machine-id file as a hard error. That strictness exists to
/// stop one user's processes deriving two different identities and each
/// believing it is the singleton. A fact reported to a caller that is only
/// comparing machines has the opposite preference: keep looking, and answer
/// with the best id available.
pub fn machine_id() -> Option<String> {
    MACHINE_ID_PATHS
        .iter()
        .find_map(|path| read_trimmed(path))
        .or_else(|| read_trimmed(BOOT_ID_PATH).map(|id| format!("boot:{id}")))
}

/// An identifier that changes on every boot of this machine.
pub fn boot_id() -> Option<String> {
    read_trimmed(BOOT_ID_PATH)
}

/// The mount and PID namespaces this process is in.
///
/// Two processes on the same machine in the same boot can still be in
/// different containers, and then they share neither a filesystem view nor a
/// PID space. That is a real identity difference, so it is reported as one.
pub fn namespace_id() -> Option<String> {
    let mnt = read_link_lossy("/proc/self/ns/mnt").unwrap_or_else(|| "mntns:unknown".to_string());
    let pid = read_link_lossy("/proc/self/ns/pid").unwrap_or_else(|| "pidns:unknown".to_string());
    Some(format!("{mnt}:{pid}"))
}

const MACHINE_ID_PATHS: [&str; 2] = ["/etc/machine-id", "/var/lib/dbus/machine-id"];
const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_link_lossy(path: &str) -> Option<String> {
    std::fs::read_link(path)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
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
