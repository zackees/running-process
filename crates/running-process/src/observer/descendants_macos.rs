//! #539 slice 7 — macOS descendant-lifecycle backend.
//!
//! **History:** the first cut of this module used `kqueue` +
//! `EVFILT_PROC` + `NOTE_TRACK`. Empirically on the macos-arm CI
//! runner, NOTE_TRACK silently failed to emit `NOTE_CHILD` events
//! for spawned descendants — a long-standing reliability issue with
//! NOTE_TRACK on modern macOS that Apple has not addressed (the
//! recommended replacement is Endpoint Security, which requires the
//! `com.apple.developer.endpoint-security.client` entitlement and is
//! out of scope for the no-admin LaunchedProcessTree tier). After
//! the integration test failed twice with `got 0 (all: [])` despite
//! synchronous registration before the spawn race window, we pivoted
//! to the same polling shape Linux uses.
//!
//! **Current implementation:** snapshot every process on the system
//! via `sysctl({CTL_KERN, KERN_PROC, KERN_PROC_ALL})` every 50 ms,
//! build a parent → children map, BFS from the root PID, diff
//! against the previous snapshot, and emit
//! [`DescendantStarted`](crate::observer::ObserverEventKind::DescendantStarted)
//! / [`DescendantExited`](crate::observer::ObserverEventKind::DescendantExited)
//! on the consumer's [`ObserverSubscriber`].
//!
//! Tradeoffs vs. Endpoint Security:
//!
//! - **No entitlement required.** Works against any process the
//!   calling user owns.
//! - **Polling-based**: short-lived descendants that spawn and exit
//!   within the same 50 ms window may be missed. Same honesty caveat
//!   as the Linux `/proc` poll.
//! - **Per-snapshot cost**: one `sysctl()` walk of the full process
//!   table. Typically a few hundred entries; cheap.

#![cfg(target_os = "macos")]

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::observer::{EventCategory, ObserverEvent, ObserverEventKind};

const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Spawn the descendant-tracking pump thread for `root_pid`. Returns
/// silently after spawning — the thread terminates when `root_pid`
/// disappears from the global process table.
pub(crate) fn spawn_pump(root_pid: u32, sink: Sender<ObserverEvent>) {
    let _ = std::thread::Builder::new()
        .name("rp-macos-descpump".to_string())
        .spawn(move || pump_loop(root_pid, sink));
}

fn pump_loop(root_pid: u32, sink: Sender<ObserverEvent>) {
    let mut known: HashSet<u32> = HashSet::new();
    loop {
        let all = list_all_processes();
        if !all.iter().any(|&(pid, _)| pid == root_pid) {
            // Root is gone — emit exits for any remaining tracked
            // descendants and terminate. Mirrors the Linux pump's
            // /proc-disappearance termination condition.
            for &pid in &known {
                let _ = sink.send(ObserverEvent::new_now(
                    EventCategory::Process,
                    ObserverEventKind::DescendantExited,
                    pid,
                ));
            }
            break;
        }
        let current = descendants_of(root_pid, &all);
        emit_diff(&known, &current, &sink);
        known = current;
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Snapshot every process on the system, returning a `Vec<(pid, ppid)>`.
///
/// Uses `proc_listpids(PROC_ALL_PIDS)` to enumerate PIDs, then
/// `proc_pidinfo(pid, PROC_PIDTBSDINFO)` to look up each PPID. This
/// avoids depending on `libc::kinfo_proc` (which our pinned libc
/// 0.2 does not export on macOS targets) and is the documented
/// Apple API for cross-process introspection.
fn list_all_processes() -> Vec<(u32, u32)> {
    // proc_listpids size probe — pass null buffer to learn the
    // required size in bytes.
    let size = unsafe {
        libc::proc_listpids(
            libc::PROC_ALL_PIDS,
            0,
            std::ptr::null_mut(),
            0,
        )
    };
    if size <= 0 {
        return Vec::new();
    }
    let pid_count = (size as usize) / std::mem::size_of::<libc::pid_t>();
    if pid_count == 0 {
        return Vec::new();
    }
    let mut pids: Vec<libc::pid_t> = vec![0; pid_count];
    let written_bytes = unsafe {
        libc::proc_listpids(
            libc::PROC_ALL_PIDS,
            0,
            pids.as_mut_ptr() as *mut libc::c_void,
            (pid_count * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        )
    };
    if written_bytes <= 0 {
        return Vec::new();
    }
    let written = (written_bytes as usize) / std::mem::size_of::<libc::pid_t>();
    pids.truncate(written);

    let mut result = Vec::with_capacity(written);
    for &pid in &pids {
        if pid <= 0 {
            continue;
        }
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut libc::proc_bsdinfo as *mut libc::c_void,
                std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int,
            )
        };
        // proc_pidinfo returns the number of bytes written; 0 means
        // the process disappeared between listpids and the info
        // query. Skip those races.
        if n <= 0 {
            continue;
        }
        result.push((info.pbi_pid, info.pbi_ppid));
    }
    result
}

/// BFS the descendant subtree of `root_pid` given the full
/// `(pid, ppid)` snapshot. Returns the set of every transitive
/// descendant (the root itself is excluded).
fn descendants_of(root_pid: u32, all: &[(u32, u32)]) -> HashSet<u32> {
    let mut child_map: HashMap<u32, Vec<u32>> = HashMap::new();
    for &(pid, ppid) in all {
        child_map.entry(ppid).or_default().push(pid);
    }
    let mut result = HashSet::new();
    let mut stack = vec![root_pid];
    while let Some(pid) = stack.pop() {
        if let Some(children) = child_map.get(&pid) {
            for &c in children {
                if result.insert(c) {
                    stack.push(c);
                }
            }
        }
    }
    result
}

/// Emit DescendantStarted for `current \ prev` and DescendantExited
/// for `prev \ current`. Send errors are ignored — a dropped
/// subscriber must never crash the pump.
fn emit_diff(
    prev: &HashSet<u32>,
    current: &HashSet<u32>,
    sink: &Sender<ObserverEvent>,
) {
    for &new_pid in current.difference(prev) {
        let _ = sink.send(ObserverEvent::new_now(
            EventCategory::Process,
            ObserverEventKind::DescendantStarted,
            new_pid,
        ));
    }
    for &gone_pid in prev.difference(current) {
        let _ = sink.send(ObserverEvent::new_now(
            EventCategory::Process,
            ObserverEventKind::DescendantExited,
            gone_pid,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn emit_diff_fires_one_started_per_new_pid() {
        let (tx, rx) = mpsc::channel();
        let prev: HashSet<u32> = [10, 20].into_iter().collect();
        let current: HashSet<u32> = [10, 20, 30, 40].into_iter().collect();
        emit_diff(&prev, &current, &tx);
        drop(tx);
        let evs: Vec<_> = rx.iter().collect();
        assert_eq!(evs.len(), 2);
        let started_pids: HashSet<u32> = evs
            .iter()
            .filter(|e| matches!(e.kind, ObserverEventKind::DescendantStarted))
            .map(|e| e.pid)
            .collect();
        assert_eq!(started_pids, [30, 40].into_iter().collect::<HashSet<_>>());
    }

    #[test]
    fn emit_diff_fires_one_exited_per_gone_pid() {
        let (tx, rx) = mpsc::channel();
        let prev: HashSet<u32> = [10, 20, 30].into_iter().collect();
        let current: HashSet<u32> = [10].into_iter().collect();
        emit_diff(&prev, &current, &tx);
        drop(tx);
        let evs: Vec<_> = rx.iter().collect();
        assert_eq!(evs.len(), 2);
        let exited_pids: HashSet<u32> = evs
            .iter()
            .filter(|e| matches!(e.kind, ObserverEventKind::DescendantExited))
            .map(|e| e.pid)
            .collect();
        assert_eq!(exited_pids, [20, 30].into_iter().collect::<HashSet<_>>());
    }

    #[test]
    fn descendants_of_handles_branching_tree() {
        // Tree: 100 -> {200, 300}; 200 -> {201}; 300 has no children.
        let all = vec![(100, 0), (200, 100), (201, 200), (300, 100), (999, 1)];
        let descendants = descendants_of(100, &all);
        assert_eq!(
            descendants,
            [200, 201, 300].into_iter().collect::<HashSet<_>>()
        );
    }

    #[test]
    fn descendants_of_for_unknown_root_returns_empty() {
        let all = vec![(100, 0), (200, 100)];
        let descendants = descendants_of(0x7FFF_FFFE, &all);
        assert!(descendants.is_empty());
    }

    #[test]
    fn list_all_processes_returns_non_empty_on_real_macos() {
        // Sanity check the sysctl pipeline on the actual macos-arm
        // CI runner — there's always at least `launchd`, the test
        // process itself, plus dozens of system daemons.
        let all = list_all_processes();
        assert!(
            all.len() > 5,
            "expected the macOS process table to have plenty of entries, got {}",
            all.len()
        );
        // The current process must be in there.
        let self_pid = std::process::id();
        assert!(
            all.iter().any(|&(pid, _)| pid == self_pid),
            "expected current pid {self_pid} in process table"
        );
    }

    #[test]
    fn end_to_end_descendant_started_and_exited_for_spawned_chain() {
        use crate::observer::ObserverConfig;
        use crate::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};

        // Same fixture shape as the Linux integration test. With
        // 50 ms polling and bash totalling ~700 ms, the snapshot
        // diff catches the three background sleeps comfortably.
        let cfg = ProcessConfig {
            command: CommandSpec::Argv(vec![
                "bash".into(),
                "-c".into(),
                "sleep 0.5 & sleep 0.5 & sleep 0.5 & wait".into(),
            ]),
            cwd: None,
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
        };
        let (process, subscriber) = NativeProcess::with_observer(
            cfg,
            ObserverConfig::with_categories([EventCategory::Process]),
        );
        process.start().expect("spawn bash chain");
        let _ = process
            .wait(Some(Duration::from_secs(30)))
            .expect("bash chain exits");
        process.close().ok();
        // Give the pump time to run the final diff + emit exits and
        // hit its root-disappeared termination check.
        std::thread::sleep(Duration::from_millis(200));

        let events = subscriber.drain();
        let started = events
            .iter()
            .filter(|e| {
                e.category == EventCategory::Process
                    && matches!(e.kind, ObserverEventKind::DescendantStarted)
            })
            .count();
        let exited = events
            .iter()
            .filter(|e| {
                e.category == EventCategory::Process
                    && matches!(e.kind, ObserverEventKind::DescendantExited)
            })
            .count();
        assert!(
            started >= 3,
            "expected ≥3 DescendantStarted, got {started} (all: {events:?})"
        );
        assert!(
            exited >= 3,
            "expected ≥3 DescendantExited, got {exited} (all: {events:?})"
        );
        for ev in &events {
            assert_eq!(
                ev.category,
                EventCategory::Process,
                "Lifecycle leaked into Process-only subscriber: {ev:?}"
            );
        }
    }
}
