//! #539 slice 5 — Linux descendant-lifecycle backend.
//!
//! No-admin Linux primitive: enable `PR_SET_CHILD_SUBREAPER` so orphaned
//! descendants reparent to us (not to init), and run a background pump
//! that snapshots `/proc/<pid>/task/<pid>/children` every 50 ms,
//! diffing against the previous snapshot to emit
//! [`DescendantStarted`](crate::observer::ObserverEventKind::DescendantStarted)
//! / [`DescendantExited`](crate::observer::ObserverEventKind::DescendantExited)
//! on the consumer's [`ObserverSubscriber`].
//!
//! Tradeoffs vs. eBPF / cn_proc:
//!
//! - **No CAP_BPF / no CAP_NET_ADMIN required.** Works on stock kernels
//!   from any non-elevated process.
//! - **Polling-based**: short-lived descendants that spawn and exit
//!   within the same 50 ms window may be missed. This is the same
//!   tradeoff `proc_pidinfo`-based macOS snapshots make and is the only
//!   honest no-admin option on Linux.
//! - **Subreaper is process-wide**: `prctl(PR_SET_CHILD_SUBREAPER, 1)`
//!   affects the whole calling process. Setting it idempotently is
//!   safe; we never clear it.

#![cfg(target_os = "linux")]

use std::collections::HashSet;
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::observer::{EventCategory, ObserverEvent, ObserverEventKind};

/// Poll interval for the /proc descendant snapshot. 50 ms is the same
/// cadence we'd expect a debug UI to refresh at, and matches the
/// short-lived-descendant honesty caveat in this module's docs.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Enable `PR_SET_CHILD_SUBREAPER` so orphaned descendants of any
/// process this process spawns reparent to us instead of init. Safe to
/// call repeatedly — `prctl` is idempotent here.
///
/// Errors are deliberately swallowed: if subreaper can't be set (e.g.
/// inside a sandbox), the pump still works for descendants whose
/// immediate parent stays alive — we just lose long-tail tracking of
/// orphaned descendants. The matrix advertised behavior is still
/// honored.
pub(crate) fn enable_subreaper() {
    // SAFETY: `prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0)` is a leaf
    // syscall with no pointer arguments; cannot violate Rust aliasing.
    let _ = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
}

/// Spawn the descendant-tracking pump thread for `root_pid`. Returns
/// silently after spawning — the thread terminates when `root_pid`
/// exits.
pub(crate) fn spawn_pump(root_pid: u32, sink: Sender<ObserverEvent>) {
    let _ = std::thread::Builder::new()
        .name("rp-linux-descpump".to_string())
        .spawn(move || pump_loop(root_pid, sink));
}

/// Walk `/proc/<pid>/task/<pid>/children` recursively, returning every
/// transitive descendant PID of `root_pid`. Robust to mid-walk exits:
/// a missing `children` file just truncates that branch of the walk.
fn descendant_pids(root_pid: u32) -> Vec<u32> {
    let mut result = Vec::new();
    let mut stack: Vec<u32> = vec![root_pid];
    while let Some(pid) = stack.pop() {
        let path = format!("/proc/{pid}/task/{pid}/children");
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        for token in contents.split_ascii_whitespace() {
            if let Ok(child) = token.parse::<u32>() {
                result.push(child);
                stack.push(child);
            }
        }
    }
    result
}

/// The pump loop. Snapshots descendants every [`POLL_INTERVAL`], diffs
/// against the previous snapshot to emit `DescendantStarted` for new
/// PIDs and `DescendantExited` for missing PIDs. Terminates when the
/// root process is gone, emitting `DescendantExited` for any
/// still-tracked descendants on the way out.
fn pump_loop(root_pid: u32, sink: Sender<ObserverEvent>) {
    let mut known: HashSet<u32> = HashSet::new();
    let root_path = format!("/proc/{root_pid}");
    loop {
        // Exit when the root is gone — the pump's contract is bounded
        // by the spawned tree's lifetime, mirroring the Windows IOCP
        // pump's ACTIVE_PROCESS_ZERO termination semantics.
        if !std::path::Path::new(&root_path).exists() {
            break;
        }
        let current: HashSet<u32> = descendant_pids(root_pid).into_iter().collect();
        emit_diff(&known, &current, &sink);
        known = current;
        std::thread::sleep(POLL_INTERVAL);
    }
    // Root exited: surface any still-tracked descendants as exited so
    // the consumer's started/exited counts stay balanced.
    for &pid in &known {
        let _ = sink.send(ObserverEvent::new_now(
            EventCategory::Process,
            ObserverEventKind::DescendantExited,
            pid,
        ));
    }
}

/// Emit DescendantStarted for `current \ prev` and DescendantExited
/// for `prev \ current`. Send errors are ignored — a dropped
/// subscriber must never crash the pump.
fn emit_diff(prev: &HashSet<u32>, current: &HashSet<u32>, sink: &Sender<ObserverEvent>) {
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
    fn emit_diff_no_events_when_steady_state() {
        let (tx, rx) = mpsc::channel();
        let prev: HashSet<u32> = [10, 20].into_iter().collect();
        let current = prev.clone();
        emit_diff(&prev, &current, &tx);
        drop(tx);
        assert_eq!(rx.iter().count(), 0);
    }

    #[test]
    fn descendant_pids_for_nonexistent_root_returns_empty() {
        // /proc/<missing>/task/<missing>/children won't exist — the
        // walk should terminate cleanly with an empty result, not panic.
        let pids = descendant_pids(0x7FFF_FFFE);
        assert!(pids.is_empty(), "expected no descendants, got {pids:?}");
    }

    #[test]
    fn descendant_pids_for_self_includes_no_phantom_entries() {
        // For a process that has no children right now (test thread),
        // the walk returns either an empty list or only well-known
        // children we just spawned. We just assert it doesn't panic
        // and the returned PIDs all look plausible (non-zero).
        let pids = descendant_pids(std::process::id());
        for pid in pids {
            assert!(pid > 1, "pid {pid} is suspiciously small");
        }
    }

    #[test]
    fn end_to_end_descendant_started_and_exited_for_spawned_chain() {
        use crate::observer::ObserverConfig;
        use crate::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};

        // Direct child: bash that spawns 3 sleepers in the background
        // then waits on them. Each background sleep is a descendant
        // (subprocess of bash), so we expect ≥3 DescendantStarted
        // followed by ≥3 DescendantExited as they run to completion.
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

        // The pump terminates once root is gone and flushes pending
        // exits; give it a beat past the poll interval to settle.
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
            "expected ≥3 DescendantStarted events, got {started} (all: {events:?})"
        );
        assert!(
            exited >= 3,
            "expected ≥3 DescendantExited events, got {exited} (all: {events:?})"
        );
        // Only Process events should appear — Lifecycle wasn't requested.
        for ev in &events {
            assert_eq!(
                ev.category,
                EventCategory::Process,
                "Lifecycle leaked into Process-only subscriber: {ev:?}"
            );
        }
    }
}
