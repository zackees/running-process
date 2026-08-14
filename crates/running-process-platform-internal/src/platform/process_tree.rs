//! Host-neutral process-tree traversal shared by selected platform roots.

use std::collections::HashSet;
use std::io;
use std::time::{Duration, Instant};

use sysinfo::{Pid, Process, System};

pub(crate) type ProcessStartKey = fn(Pid, &Process) -> io::Result<u64>;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ProcessInstance {
    pid: Pid,
    start_time: u64,
}

impl ProcessInstance {
    fn from_process(pid: Pid, process: &Process, start_key: ProcessStartKey) -> io::Result<Self> {
        Ok(Self { pid, start_time: start_key(pid, process)? })
    }

    fn still_matches(self, system: &System, start_key: ProcessStartKey) -> bool {
        system.process(self.pid).is_some_and(|process| {
            start_key(self.pid, process).is_ok_and(|time| time == self.start_time)
        })
    }
}

pub(crate) fn kill_tree(pid: u32, timeout: Duration, start_key: ProcessStartKey) -> io::Result<u32> {
    let mut system = System::new();
    system.refresh_processes();
    let root_pid = Pid::from_u32(pid);
    let Some(root_process) = system.process(root_pid) else { return Ok(0); };
    let root = match ProcessInstance::from_process(root_pid, root_process, start_key) {
        Ok(root) => root,
        Err(error) => {
            system.refresh_processes();
            if system.process(root_pid).is_none() { return Ok(0); }
            return Err(error);
        }
    };

    let mut descendants = Vec::new();
    collect_descendants(&system, root, 1, &mut HashSet::new(), &mut descendants, start_key);
    descendants.sort_unstable_by_key(|(_, depth)| std::cmp::Reverse(*depth));
    let mut targets = Vec::with_capacity(descendants.len() + 1);
    targets.push(root);
    targets.extend(descendants.into_iter().map(|(instance, _)| instance));

    let mut signaled = HashSet::new();
    signal_matching(&system, &targets, &mut signaled, start_key);
    let started = Instant::now();
    loop {
        system.refresh_processes();
        let remaining: Vec<_> = targets.iter().copied()
            .filter(|target| target.still_matches(&system, start_key)).collect();
        if remaining.is_empty() || started.elapsed() >= timeout { break; }
        signal_matching(&system, &remaining, &mut signaled, start_key);
        let sleep_for = timeout.saturating_sub(started.elapsed()).min(Duration::from_millis(25));
        if sleep_for.is_zero() { break; }
        std::thread::sleep(sleep_for);
    }
    Ok(signaled.len() as u32)
}

fn signal_matching(
    system: &System,
    targets: &[ProcessInstance],
    signaled: &mut HashSet<ProcessInstance>,
    start_key: ProcessStartKey,
) {
    for target in targets {
        let Some(process) = system.process(target.pid) else { continue; };
        if start_key(target.pid, process).is_ok_and(|time| time == target.start_time)
            && process.kill()
        {
            signaled.insert(*target);
        }
    }
}

fn collect_descendants(
    system: &System,
    parent: ProcessInstance,
    depth: usize,
    visited: &mut HashSet<Pid>,
    descendants: &mut Vec<(ProcessInstance, usize)>,
    start_key: ProcessStartKey,
) {
    for (pid, process) in system.processes() {
        if process.parent() != Some(parent.pid) || visited.contains(pid) { continue; }
        let Ok(child) = ProcessInstance::from_process(*pid, process, start_key) else { continue; };
        if child.start_time < parent.start_time { continue; }
        visited.insert(*pid);
        descendants.push((child, depth));
        collect_descendants(system, child, depth + 1, visited, descendants, start_key);
    }
}
