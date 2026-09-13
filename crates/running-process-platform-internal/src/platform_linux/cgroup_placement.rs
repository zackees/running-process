//! Strict cgroup-v2 placement evidence for an externally launched process.

use std::io::{self, Read};
use std::path::{Component, Path};

/// Verify that `pid` is outside this process's visible unified cgroup subtree.
///
/// This does not bypass limits on shared ancestors. Hybrid layouts with a
/// legacy memory controller are rejected: v2 membership alone cannot prove
/// memory-accounting separation on those hosts.
pub fn verify_process_cgroup_separation(pid: u32) -> io::Result<()> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid process identifier"));
    }
    let caller = read_membership(Path::new("/proc/self/cgroup"))?;
    let target = read_membership(Path::new(&format!("/proc/{pid}/cgroup")))?;
    verify_membership(&caller, &target)
}

fn read_membership(path: &Path) -> io::Result<String> {
    const LIMIT: u64 = 64 * 1024;
    let mut text = String::new();
    std::fs::File::open(path)?.take(LIMIT + 1).read_to_string(&mut text)?;
    if text.len() as u64 > LIMIT {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "oversized cgroup membership"));
    }
    Ok(text)
}

fn unified_path(text: &str) -> io::Result<&Path> {
    let mut unified = None;
    for line in text.lines() {
        let mut fields = line.splitn(3, ':');
        let hierarchy = fields.next().unwrap_or_default();
        let controllers = fields.next().ok_or_else(invalid_membership)?;
        let path = fields.next().ok_or_else(invalid_membership)?;
        if controllers.split(',').any(|controller| controller == "memory") {
            return Err(io::Error::new(io::ErrorKind::Unsupported,
                "legacy memory controller prevents verified unified memory placement"));
        }
        if hierarchy != "0" { continue; }
        if !controllers.is_empty() || unified.is_some() { return Err(invalid_membership()); }
        let path = Path::new(path);
        if !path.is_absolute() || path.components().any(|part| matches!(part, Component::ParentDir)) {
            return Err(io::Error::new(io::ErrorKind::Unsupported,
                "cgroup namespace does not expose verifiable absolute placement"));
        }
        unified = Some(path);
    }
    unified.ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported,
        "independent placement requires a visible unified cgroup hierarchy"))
}

fn invalid_membership() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid cgroup membership")
}

fn verify_membership(caller: &str, target: &str) -> io::Result<()> {
    let caller = unified_path(caller)?;
    let target = unified_path(target)?;
    if target.starts_with(caller) {
        return Err(io::Error::other("launched process remains in the caller's containment subtree"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_same_group_descendants_and_root() {
        for (caller, target) in [("/worker", "/worker"), ("/worker", "/worker/daemon"), ("/", "/daemon")] {
            assert!(verify_membership(&format!("0::{caller}\n"), &format!("0::{target}\n")).is_err());
        }
    }

    #[test]
    fn accepts_siblings_without_string_prefix_confusion() {
        assert!(verify_membership("0::/worker\n", "0::/worker-other\n").is_ok());
        assert!(verify_membership("0::/user/worker\n", "0::/user/daemon\n").is_ok());
    }

    #[test]
    fn hybrid_memory_accounting_is_not_proven_by_unified_membership() {
        let hybrid = "5:cpu,memory:/worker\n0::/daemon\n";
        assert_eq!(verify_membership("0::/worker\n", hybrid).unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert_eq!(verify_membership(hybrid, "0::/sibling\n").unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert_eq!(unified_path("5:memory:/worker\n").unwrap_err().kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn refuses_ambiguous_and_namespace_relative_membership() {
        for text in ["0::/worker\n0::/other\n", "0:memory:/worker\n", "0::relative\n", "0::/../outside\n", "malformed\n"] {
            assert!(unified_path(text).is_err());
        }
    }
}
