//! Cgroup-v2 evidence shared by scheduler and external-broker launching.
//! Snapshots prove placement at observation time, not process identity or
//! readiness. Launchers must retain a process handle and verify identity too.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Placement {
    path: PathBuf,
    cgroup_namespace: PathBuf,
    mount_namespace: PathBuf,
}

impl Placement {
    /// Check a live kernel identity on both sides of the procfs reads. If the
    /// original process exited, its PID might have been reused and the snapshot
    /// must not be accepted, even if the replacement happens to be outside.
    pub(super) fn capture_pinned(
        process: &super::process_inspect::ProcessLiveness,
    ) -> io::Result<Self> {
        process.signal_pinned(0)?;
        let placement = Self::capture(process.pid())?;
        process.signal_pinned(0)?;
        if !process.is_alive() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "process exited during placement verification",
            ));
        }
        Ok(placement)
    }

    pub(super) fn capture(pid: u32) -> io::Result<Self> {
        if pid == 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let directory = PathBuf::from(format!("/proc/{pid}"));
        let cgroup_namespace = std::fs::read_link(directory.join("ns/cgroup"))?;
        let mount_namespace = std::fs::read_link(directory.join("ns/mnt"))?;
        let cgroups = std::fs::read_to_string(directory.join("cgroup"))?;
        if std::fs::read_link(directory.join("ns/cgroup"))? != cgroup_namespace
            || std::fs::read_link(directory.join("ns/mnt"))? != mount_namespace
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "process changed namespaces during placement capture",
            ));
        }
        Self::parse(&cgroups, cgroup_namespace, mount_namespace)
    }

    pub(super) fn parse(
        cgroups: &str,
        cgroup_namespace: PathBuf,
        mount_namespace: PathBuf,
    ) -> io::Result<Self> {
        let path = cgroups
            .strip_prefix("0::")
            .and_then(|value| value.strip_suffix('\n'))
            .ok_or_else(|| io::Error::from(io::ErrorKind::Unsupported))?;
        if !path.starts_with('/')
            || path.contains(['\n', '\r', '\0'])
            || (path != "/"
                && path
                    .split('/')
                    .skip(1)
                    .any(|part| matches!(part, "" | "." | "..")))
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "ambiguous cgroup-v2 membership",
            ));
        }
        Ok(Self {
            path: PathBuf::from(path),
            cgroup_namespace,
            mount_namespace,
        })
    }

    pub(super) fn outside_worker(&self, worker: &Self) -> io::Result<bool> {
        if self.cgroup_namespace != worker.cgroup_namespace
            || self.mount_namespace != worker.mount_namespace
            || worker.path == Path::new("/")
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "no comparable worker cgroup boundary",
            ));
        }
        Ok(!self.path.starts_with(&worker.path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placement(path: &str) -> Placement {
        Placement::parse(
            &format!("0::{path}\n"),
            PathBuf::from("cgroup:[17]"),
            PathBuf::from("mnt:[23]"),
        )
        .unwrap()
    }

    #[test]
    fn sibling_is_outside_but_equal_and_descendant_are_contained() {
        let worker = placement("/user.slice/worker.scope");
        assert!(placement("/user.slice/daemon.service")
            .outside_worker(&worker)
            .unwrap());
        assert!(!worker.outside_worker(&worker).unwrap());
        assert!(!placement("/user.slice/worker.scope/child")
            .outside_worker(&worker)
            .unwrap());
    }

    #[test]
    fn comparison_uses_path_components_not_string_prefixes() {
        assert!(placement("/worker-2")
            .outside_worker(&placement("/worker"))
            .unwrap());
    }

    #[test]
    fn root_is_not_a_worker_boundary() {
        assert_eq!(
            placement("/daemon")
                .outside_worker(&placement("/"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[test]
    fn namespaces_must_match_before_comparing_paths() {
        let worker = placement("/worker");
        let mut other = placement("/daemon");
        other.cgroup_namespace = PathBuf::from("cgroup:[18]");
        assert_eq!(
            other.outside_worker(&worker).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        other.cgroup_namespace = worker.cgroup_namespace.clone();
        other.mount_namespace = PathBuf::from("mnt:[24]");
        assert_eq!(
            other.outside_worker(&worker).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[test]
    fn ambiguous_or_non_unified_membership_is_rejected() {
        for text in [
            "",
            "0::relative\n",
            "0::/a/../b\n",
            "0::/a/./b\n",
            "0::/a//b\n",
            "0::/a\n0::/b\n",
            "2:memory:/a\n",
            "0::/a\n2:memory:/b\n",
        ] {
            assert!(
                Placement::parse(text, "cgroup:[17]".into(), "mnt:[23]".into()).is_err(),
                "{text:?}"
            );
        }
    }

    #[test]
    fn live_capture_compares_self_as_contained() {
        let current = Placement::capture(std::process::id()).unwrap();
        if current.path == Path::new("/") {
            assert_eq!(
                current.outside_worker(&current).unwrap_err().kind(),
                io::ErrorKind::Unsupported
            );
        } else {
            assert!(!current.outside_worker(&current).unwrap());
        }
    }

    #[test]
    fn pid_zero_is_not_the_current_process() {
        assert_eq!(
            Placement::capture(0).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
