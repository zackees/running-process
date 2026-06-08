//! Host identity values stored in v1 CacheManifest files.
//!
//! Phase 2 of #228 (#231). The cleanup tool uses this identity to skip
//! manifests restored from another machine or from a prior boot.

use std::path::Path;

use crate::broker::protocol::HostIdentity;

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
        hostname: hostname(),
        machine_id: machine_id(),
        boot_id: boot_id(),
        fs_dev_id: fs_dev_id(path),
        namespace_id: namespace_id(),
    }
}

fn hostname() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".to_string())
    }
    #[cfg(unix)]
    {
        unix_hostname()
    }
}

fn machine_id() -> String {
    #[cfg(target_os = "linux")]
    {
        read_trimmed("/etc/machine-id")
            .or_else(|| read_trimmed("/var/lib/dbus/machine-id"))
            .unwrap_or_else(|| "unknown".to_string())
    }
    #[cfg(target_os = "macos")]
    {
        // The final broker platform module will use IOPlatformUUID.
        // Phase 2 avoids spawning `ioreg` so the cleanup-only client
        // path passes the repo's spawn-path guard.
        format!("macos-{}", unix_hostname())
    }
    #[cfg(windows)]
    {
        // Avoid a registry dependency in the client feature. The
        // hostname fallback is stable enough for Phase 2 cleanup
        // filtering; a later security-hardening PR can replace this
        // with MachineGuid once the broker's Windows platform module
        // exists.
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".to_string())
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        "unknown".to_string()
    }
}

fn boot_id() -> String {
    #[cfg(target_os = "linux")]
    {
        read_trimmed("/proc/sys/kernel/random/boot_id").unwrap_or_else(|| "unknown".to_string())
    }
    #[cfg(target_os = "macos")]
    {
        macos_boot_time()
    }
    #[cfg(windows)]
    {
        use std::time::{Duration, SystemTime, UNIX_EPOCH};

        let uptime = unsafe { winapi::um::sysinfoapi::GetTickCount64() };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0));
        let boot = now.saturating_sub(Duration::from_millis(uptime));
        format!("windows-boot-{}", boot.as_secs())
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        "unknown".to_string()
    }
}

fn namespace_id() -> String {
    #[cfg(target_os = "linux")]
    {
        let mnt = std::fs::read_link("/proc/self/ns/mnt")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "mntns:unknown".to_string());
        let pid = std::fs::read_link("/proc/self/ns/pid")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "pidns:unknown".to_string());
        format!("{mnt}:{pid}")
    }
    #[cfg(not(target_os = "linux"))]
    {
        String::new()
    }
}

#[cfg(unix)]
fn fs_dev_id(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;

    std::fs::metadata(path).map(|m| m.dev()).unwrap_or(0)
}

#[cfg(windows)]
fn fs_dev_id(_path: &Path) -> u64 {
    0
}

#[cfg(target_os = "linux")]
fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(unix)]
fn unix_hostname() -> String {
    let mut buf = [0_u8; 256];
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if ok != 0 {
        return "unknown".to_string();
    }
    let nul = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..nul]).into_owned()
}

#[cfg(target_os = "macos")]
fn macos_boot_time() -> String {
    use std::ffi::CString;

    let name = CString::new("kern.boottime").expect("static sysctl name");
    let mut boot: libc::timeval = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::timeval>();
    let ok = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut boot as *mut libc::timeval).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        format!("macos-boot-{}-{}", boot.tv_sec, boot.tv_usec)
    } else {
        "unknown".to_string()
    }
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
}
