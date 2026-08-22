//! Filesystem-health probes for status/doctor visibility (#390).
//!
//! Inode usage matters where a filesystem has a fixed inode table (ext4 most
//! prominently): the daemon data dir can fail writes with `ENOSPC` while
//! plenty of bytes remain free. Which hosts and filesystems have one to report
//! is [`crate::platform::resources`]'s answer; what this module owns is where
//! to probe and how to present the result.

use std::path::Path;

/// Inode totals for one filesystem, from `statvfs` on Unix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InodeUsage {
    /// Total inodes on the filesystem (`f_files`).
    pub total: u64,
    /// Inodes available to unprivileged users (`f_favail`).
    pub free: u64,
}

impl InodeUsage {
    /// Inodes currently in use.
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }

    /// Used fraction in `[0.0, 1.0]`; `0.0` when the total is zero.
    pub fn used_ratio(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.used() as f64 / self.total as f64
        }
    }
}

/// Probe inode usage for the filesystem containing `path`.
///
/// Returns `Ok(None)` when inode accounting does not apply -- a host with no
/// fixed inode table, or a filesystem that reports an empty one. Errors are
/// real probe failures (missing path, permission denied).
pub fn inode_usage(path: &Path) -> std::io::Result<Option<InodeUsage>> {
    Ok(
        crate::platform::resources::inode_capacity(path)?.map(|capacity| InodeUsage {
            total: capacity.total,
            free: capacity.free,
        }),
    )
}

/// Probe inode usage for the daemon data directory (where the SQLite
/// tracking database lives), walking up to the nearest existing ancestor
/// so the probe stays read-only even before the daemon ever ran.
pub fn daemon_data_dir_inode_usage() -> std::io::Result<Option<InodeUsage>> {
    let dir = crate::client::paths::data_dir();
    let mut probe: &Path = &dir;
    while !probe.exists() {
        match probe.parent() {
            Some(parent) => probe = parent,
            None => break,
        }
    }
    inode_usage(probe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn used_ratio_handles_zero_total() {
        let usage = InodeUsage { total: 0, free: 0 };
        assert_eq!(usage.used_ratio(), 0.0);
    }

    #[test]
    fn used_ratio_is_fractional() {
        let usage = InodeUsage {
            total: 100,
            free: 25,
        };
        assert_eq!(usage.used(), 75);
        assert!((usage.used_ratio() - 0.75).abs() < f64::EPSILON);
    }

    /// Whatever this host reports for a directory that exists, it is either a
    /// coherent usage or an explicit "not applicable" -- never an error. Which
    /// of the two is the host's business, and is asserted where the host is.
    #[test]
    fn inode_usage_probes_temp_dir() {
        let result = inode_usage(&std::env::temp_dir()).expect("probing temp dir must succeed");
        if let Some(usage) = result {
            assert!(usage.total > 0);
            assert!(usage.free <= usage.total);
            assert!(usage.used() <= usage.total);
        }
    }

    #[test]
    fn daemon_data_dir_probe_never_panics() {
        let _ = daemon_data_dir_inode_usage();
    }
}
