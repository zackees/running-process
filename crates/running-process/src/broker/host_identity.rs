//! Host identity values stored in v1 CacheManifest files.
//!
//! Phase 2 of #228 (#231). The cleanup tool uses this identity to skip
//! manifests restored from another machine or from a prior boot.
//!
//! The facts themselves come from [`crate::platform::host`]. What is decided
//! here is what their *absence* means, which is a property of the comparison
//! this identity exists to support, not of any host.

use std::path::Path;

use running_process_protocol::broker::v1::HostIdentity;

/// Return the current host identity using the current directory as the
/// filesystem-device probe.
pub fn current() -> HostIdentity {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    current_for_path(&cwd)
}

/// Return the current host identity, including the filesystem device id
/// for `path` when the platform exposes it.
pub fn current_for_path(path: &Path) -> HostIdentity {
    HostIdentity {
        hostname: crate::platform::host::hostname().unwrap_or_else(unknown),
        machine_id: crate::platform::host::machine_id().unwrap_or_else(unknown),
        boot_id: crate::platform::host::boot_id().unwrap_or_else(unavailable_boot_id),
        // Zero is the manifest's "no device recorded" value, and comparing
        // equal to another zero is harmless: a device id only ever narrows a
        // match that hostname and machine id already made.
        fs_dev_id: crate::platform::host::filesystem_device_id(path).unwrap_or(0),
        // Empty means "this host has no namespaces to distinguish", which is
        // the value hosts without them have always recorded.
        namespace_id: crate::platform::host::namespace_id().unwrap_or_default(),
    }
}

fn unknown() -> String {
    "unknown".to_string()
}

/// Fail closed when the host cannot name the current boot.
///
/// A shared constant here would make every process on every such host compare
/// as the same boot, which is exactly the mistake this field exists to catch.
/// The token is instead stable for this process and deliberately different in
/// the next one, so an identity probe refuses a daemon from an unknown boot
/// rather than accepting it.
///
/// The `windows-boot-` prefix is historical: Windows is the only host that has
/// ever reached this path, and manifests already on disk carry the spelling.
fn unavailable_boot_id() -> String {
    use std::sync::OnceLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN
        .get_or_init(|| {
            let created = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();
            format!("windows-boot-unavailable-{}-{created}", std::process::id())
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_identity_has_required_strings() {
        let id = current();
        assert!(!id.hostname.is_empty());
        assert!(!id.machine_id.is_empty());
        assert!(!id.boot_id.is_empty());
    }

    /// A path that no host can attribute to a device still yields a usable
    /// identity -- the device is the one field allowed to be absent.
    #[test]
    fn an_unattributable_path_still_yields_the_rest_of_the_identity() {
        let id = current_for_path(Path::new(""));
        assert!(!id.hostname.is_empty());
        assert!(!id.machine_id.is_empty());
        assert!(!id.boot_id.is_empty());
    }

    #[test]
    fn unavailable_boot_id_is_stable_and_fail_closed() {
        let first = unavailable_boot_id();
        assert_eq!(unavailable_boot_id(), first);
        assert!(first.starts_with(&format!("windows-boot-unavailable-{}-", std::process::id())));
        assert_ne!(first, "unknown");
    }
}
