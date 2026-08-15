//! macOS implementation of launched-tree descendant monitoring.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::platform::process::{DescendantEvent, DescendantMonitorStop};

const RECONCILE_INTERVAL: Duration = Duration::from_millis(50);

type Identity = (u64, u64);

struct Kqueue(libc::c_int);

impl Drop for Kqueue {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

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
    let queue = unsafe { libc::kqueue() };
    let queue = (queue >= 0).then_some(Kqueue(queue));
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
        if let Some(queue) = queue.as_ref() {
            register_process_hint(queue.0, root_pid);
            for &pid in &current {
                register_process_hint(queue.0, pid);
            }
        }
        known = current;
        if wait_for_hint(queue.as_ref(), &stop) {
            return;
        }
    }
}

fn register_process_hint(queue: libc::c_int, pid: u32) {
    let change = libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
        fflags: libc::NOTE_FORK | libc::NOTE_EXEC | libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // ESRCH is expected when a very short-lived process disappears between
    // reconciliation and registration. The snapshot grade remains explicitly
    // best-effort, so registration failure is only a missed wake-up hint.
    unsafe {
        libc::kevent(
            queue,
            &raw const change,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        );
    }
}

fn wait_for_hint(queue: Option<&Kqueue>, stop: &DescendantMonitorStop) -> bool {
    let Some(queue) = queue else {
        return stop.wait_timeout(RECONCILE_INTERVAL);
    };
    let timeout = libc::timespec {
        tv_sec: 0,
        tv_nsec: RECONCILE_INTERVAL.as_nanos() as libc::c_long,
    };
    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
    unsafe {
        libc::kevent(
            queue.0,
            std::ptr::null(),
            0,
            &raw mut event,
            1,
            &raw const timeout,
        );
    }
    stop.is_stopped()
}
