//! macOS implementation of launched-tree descendant monitoring.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::platform::process::{DescendantEvent, DescendantMonitorStop, ProcessSnapshot};

const POLL_INTERVAL: Duration = Duration::from_millis(50);

type Identity = (u64, u64);

pub fn start_descendant_monitor(
    root_pid: u32,
    stop: Arc<DescendantMonitorStop>,
    emit: Box<dyn Fn(DescendantEvent) + Send>,
) {
    let Some(identity) = process_identity(root_pid) else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("rp-macos-descpump".to_string())
        .spawn(move || pump_loop(root_pid, identity, stop, emit));
}

fn process_identity(pid: u32) -> Option<Identity> {
    super::process_snapshot_for_pid(pid)
        .map(|snapshot| (snapshot.start_time_a, snapshot.start_time_b))
}

fn descendants(root_pid: u32) -> HashSet<u32> {
    descendants_of(root_pid, &super::process_snapshot())
}

fn descendants_of(root_pid: u32, snapshots: &[ProcessSnapshot]) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for snapshot in snapshots {
        children
            .entry(snapshot.parent_pid)
            .or_default()
            .push(snapshot.pid);
    }
    let mut result = HashSet::new();
    let mut stack = vec![root_pid];
    while let Some(pid) = stack.pop() {
        if let Some(child_pids) = children.get(&pid) {
            for &child in child_pids {
                if result.insert(child) {
                    stack.push(child);
                }
            }
        }
    }
    result
}

fn snapshot(root_pid: u32, expected: Identity) -> Option<HashSet<u32>> {
    let before = process_identity(root_pid);
    let descendants = descendants(root_pid);
    verified_snapshot(expected, before, descendants, process_identity(root_pid))
}

fn verified_snapshot(
    expected: Identity,
    before: Option<Identity>,
    descendants: HashSet<u32>,
    after: Option<Identity>,
) -> Option<HashSet<u32>> {
    (before == Some(expected) && after == Some(expected)).then_some(descendants)
}

fn pump_loop(
    root_pid: u32,
    root_identity: Identity,
    stop: Arc<DescendantMonitorStop>,
    emit: Box<dyn Fn(DescendantEvent) + Send>,
) {
    let mut known = HashSet::new();
    loop {
        if stop.is_stopped() {
            return;
        }
        let Some(current) = snapshot(root_pid, root_identity) else {
            for pid in known {
                emit(DescendantEvent::Exited(pid));
            }
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
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, parent_pid: u32, start_time_b: u64) -> ProcessSnapshot {
        ProcessSnapshot {
            pid,
            parent_pid,
            start_time_a: 100,
            start_time_b,
        }
    }

    fn descendants_if_root_matches(
        root_pid: u32,
        expected: Identity,
        snapshots: &[ProcessSnapshot],
    ) -> Option<HashSet<u32>> {
        snapshots
            .iter()
            .find(|snapshot| snapshot.pid == root_pid)
            .and_then(|snapshot| {
                ((snapshot.start_time_a, snapshot.start_time_b) == expected)
                    .then(|| descendants_of(root_pid, snapshots))
            })
    }

    #[test]
    fn descendants_of_handles_branching_tree() {
        let snapshots = [
            process(100, 0, 1),
            process(200, 100, 2),
            process(201, 200, 3),
            process(300, 100, 4),
            process(999, 1, 5),
        ];
        assert_eq!(
            descendants_of(100, &snapshots),
            [200, 201, 300].into_iter().collect()
        );
    }

    #[test]
    fn descendants_of_for_unknown_root_returns_empty() {
        let snapshots = [process(100, 0, 1), process(200, 100, 2)];
        assert!(descendants_of(0x7fff_fffe, &snapshots).is_empty());
    }

    #[test]
    fn list_all_processes_returns_non_empty_on_real_macos() {
        let snapshots = super::super::process_snapshot();
        assert!(snapshots.len() > 5);
        assert!(snapshots
            .iter()
            .any(|snapshot| snapshot.pid == std::process::id()));
    }

    #[test]
    fn reused_root_pid_identity_mismatch_terminates_snapshot() {
        let expected = (100, 1);
        let recycled = [process(100, 0, 99), process(200, 100, 2)];
        assert_eq!(descendants_if_root_matches(100, expected, &recycled), None);
    }

    #[test]
    fn root_identity_change_after_walk_rejects_mixed_snapshot() {
        let expected = (100, 1);
        let recycled = (100, 99);
        assert_eq!(
            verified_snapshot(
                expected,
                Some(expected),
                [42].into_iter().collect(),
                Some(recycled),
            ),
            None
        );
    }
}
