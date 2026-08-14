//! macOS implementation of launched-tree descendant monitoring.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::platform::process::{DescendantEvent, DescendantMonitorStop};

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
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for snapshot in super::process_snapshot() {
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
    if process_identity(root_pid) != Some(expected) {
        return None;
    }
    let descendants = descendants(root_pid);
    (process_identity(root_pid) == Some(expected)).then_some(descendants)
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
