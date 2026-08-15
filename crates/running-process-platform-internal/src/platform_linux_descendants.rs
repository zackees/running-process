//! Linux implementation of launched-tree descendant monitoring.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::platform::process::{DescendantEvent, DescendantMonitorStop};

const POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessIdentity {
    start_ticks: u64,
}

pub fn start_descendant_monitor(
    root_pid: u32,
    stop: Arc<DescendantMonitorStop>,
    emit: Box<dyn Fn(DescendantEvent) + Send>,
) -> std::io::Result<()> {
    let Some(root_identity) = process_identity(root_pid) else {
        emit(DescendantEvent::Completed);
        return Ok(());
    };
    enable_subreaper();
    std::thread::Builder::new()
        .name("rp-linux-descpump".to_string())
        .spawn(move || pump_loop(root_pid, root_identity, stop, emit))
        .map(|_| ())
        .map_err(|error| std::io::Error::other(format!("spawn descendant monitor: {error}")))
}

fn enable_subreaper() {
    // Failure is intentionally best-effort: the monitor can still track a
    // descendant while its immediate parent remains alive.
    let _ = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
}

fn parse_start_ticks(stat: &str) -> Option<u64> {
    let suffix = stat.get(stat.rfind(')')? + 1..)?;
    suffix
        .split_ascii_whitespace()
        .nth(19)
        .and_then(|field| field.parse().ok())
}

fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some(ProcessIdentity {
        start_ticks: parse_start_ticks(&stat)?,
    })
}

fn descendant_pids(root_pid: u32) -> HashSet<u32> {
    let mut result = HashSet::new();
    let mut stack = vec![root_pid];
    while let Some(pid) = stack.pop() {
        let path = format!("/proc/{pid}/task/{pid}/children");
        let Ok(contents) = std::fs::read_to_string(path) else {
            continue;
        };
        for token in contents.split_ascii_whitespace() {
            if let Ok(child) = token.parse() {
                if result.insert(child) {
                    stack.push(child);
                }
            }
        }
    }
    result
}

fn snapshot(root_pid: u32, expected: ProcessIdentity) -> Option<HashSet<u32>> {
    let before = process_identity(root_pid);
    if before != Some(expected) {
        return None;
    }
    let descendants = descendant_pids(root_pid);
    (process_identity(root_pid) == Some(expected)).then_some(descendants)
}

fn pump_loop(
    root_pid: u32,
    root_identity: ProcessIdentity,
    stop: Arc<DescendantMonitorStop>,
    emit: Box<dyn Fn(DescendantEvent) + Send>,
) {
    let mut known = HashSet::new();
    loop {
        if stop.is_stopped() {
            emit(DescendantEvent::Completed);
            return;
        }
        let Some(current) = snapshot(root_pid, root_identity) else {
            for pid in known {
                emit(DescendantEvent::Exited(pid));
            }
            emit(DescendantEvent::Completed);
            return;
        };
        for &pid in current.difference(&known) {
            emit(DescendantEvent::Started(pid));
        }
        for &pid in known.difference(&current) {
            emit(DescendantEvent::Exited(pid));
        }
        known = current;
        if stop.wait_timeout(POLL_INTERVAL) {
            emit(DescendantEvent::Completed);
            return;
        }
    }
}
