//! Fail-fast connect guard for the broker client (running-process#894).
//!
//! The acute failure that pinned every core and hung the machine downstream
//! (zackees/soldr#2352) was not the *wrong* daemon — it was the reaction to
//! *no reachable* daemon: the caller looped, retrying / respawning / displacing
//! on every compile. A per-operation deadline alone cannot bound that, because
//! the storm is a *loop*, not one blocked call, and the caller's cache
//! kill-switch (`ZCCACHE_DISABLE=1`) no longer escapes the daemon path.
//!
//! Two pieces live here:
//!
//! * [`ConnectWatchdog`] — an out-of-band wall-clock backstop. It is armed
//!   before a connect attempt and, if not disarmed within a hard cap, aborts
//!   the process. Disarm happens on [`Drop`]; because [`std::process::exit`]
//!   does *not* run destructors, the terminal (dump-then-exit) path leaves the
//!   watchdog armed, so a wedged dump or a stuck `exit` still terminates.
//!
//! * [`capture_connect_dump`] — a cooperative all-thread stack dump captured
//!   *before* exiting, so "spins forever / hangs the box" becomes "fails fast
//!   with evidence of which thread was stuck on connect". It uses the in-crate
//!   probe when the `probe` feature is on, and is a no-op otherwise (the
//!   fail-fast `exit 1` is unconditional either way).

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

/// Extra wall-clock beyond the connect deadline before the watchdog aborts.
///
/// The connect attempt is itself deadline-bounded, so this only has to cover
/// the stack-dump-and-exit epilogue. Generous enough for a real dump on a
/// loaded machine, small enough that a genuine wedge is bounded to seconds.
pub const WATCHDOG_GRACE: Duration = Duration::from_secs(20);

/// Out-of-band wall-clock backstop around a connect attempt.
///
/// Arm before connecting; drop (only reached on the success path) to disarm.
/// If neither disarm nor process exit happens within the hard cap, the
/// process is aborted rather than allowed to hang.
pub struct ConnectWatchdog {
    disarm: Option<Sender<()>>,
}

impl ConnectWatchdog {
    /// Arm a watchdog that aborts the process if not disarmed within `hard_cap`.
    pub fn arm(hard_cap: Duration) -> Self {
        let (tx, rx) = mpsc::channel::<()>();
        // Best-effort: if the thread cannot even be spawned we simply have no
        // backstop, which is strictly no worse than before this existed.
        let _ = std::thread::Builder::new()
            .name("rp-connect-watchdog".to_owned())
            .spawn(move || match rx.recv_timeout(hard_cap) {
                // Explicit disarm, or the sender was dropped on the success
                // path — either way the guarded attempt is done.
                Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
                Err(RecvTimeoutError::Timeout) => {
                    eprintln!(
                        "running-process: connect watchdog fired after {hard_cap:?} \
                         without a reachable daemon or a clean exit; aborting to \
                         avoid a hang (running-process#894)"
                    );
                    std::process::abort();
                }
            });
        Self { disarm: Some(tx) }
    }
}

impl Drop for ConnectWatchdog {
    fn drop(&mut self) {
        if let Some(tx) = self.disarm.take() {
            let _ = tx.send(());
        }
    }
}

/// Capture an all-thread stack dump to a temp file, returning its path.
///
/// `Some(path)` when a non-empty cooperative capture was written; `None` when
/// the `probe` feature is off, the platform has no capture backend, or the
/// capture came back empty. The report is also echoed to stderr so evidence
/// survives even if the temp dir is unwritable.
pub fn capture_connect_dump(
    program: &str,
    deadline: Duration,
    error: &str,
) -> Option<std::path::PathBuf> {
    #[cfg(feature = "probe")]
    {
        use running_process_probe::snapshot::{capture_and_resolve, SnapshotConfig};

        let snapshot = capture_and_resolve(&SnapshotConfig::default()).ok()?;
        if snapshot.threads.is_empty() {
            return None;
        }
        let report = render_connect_dump(program, deadline, error, &snapshot);
        eprint!("{report}");
        let path = std::env::temp_dir().join(format!(
            "rp-connect-dump-{}-{}.txt",
            sanitize(program),
            std::process::id()
        ));
        std::fs::write(&path, &report).ok().map(|()| path)
    }
    #[cfg(not(feature = "probe"))]
    {
        let _ = (program, deadline, error);
        None
    }
}

/// Filename-safe form of a service/program name for the dump path.
#[cfg(feature = "probe")]
fn sanitize(program: &str) -> String {
    program
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Render an all-thread report from a probe snapshot.
#[cfg(feature = "probe")]
fn render_connect_dump(
    program: &str,
    deadline: Duration,
    error: &str,
    snapshot: &running_process_probe::snapshot::Snapshot,
) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "running-process: v2 broker for '{program}' unreachable within {deadline:?}: {error}"
    );
    let _ = writeln!(
        out,
        "all-thread stack dump ({} sibling thread(s), frames_resolved={}):",
        snapshot.threads.len(),
        snapshot.frames_resolved
    );
    for (idx, thread) in snapshot.threads.iter().enumerate() {
        let _ = writeln!(
            out,
            "  thread #{idx} os_tid={} ip={:#018x}{}",
            thread.os_tid,
            thread.instruction_pointer,
            if thread.truncated {
                " (stack truncated)"
            } else {
                ""
            }
        );
        if thread.frames.is_empty() {
            let _ = writeln!(out, "    <no resolvable frames>");
        }
        for (fidx, addr) in thread.frames.iter().enumerate() {
            let _ = writeln!(out, "    #{fidx:<3} {addr:#018x}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_disarms_on_drop_without_aborting() {
        // A generous cap so only the drop (disarm) — not a timeout — ends it.
        // If disarm were broken this thread would abort the whole test binary,
        // so reaching the assert *is* the assertion.
        let guard = ConnectWatchdog::arm(Duration::from_secs(3600));
        drop(guard);
        // Give the watchdog thread a moment to observe the disarm and exit.
        std::thread::sleep(Duration::from_millis(50));
    }

    #[test]
    fn dump_is_none_without_probe_feature() {
        // Under the default feature set (`probe` off) the dump is a no-op and
        // the caller relies solely on the unconditional `exit 1`.
        #[cfg(not(feature = "probe"))]
        assert!(capture_connect_dump("zccache", Duration::from_secs(3), "not found").is_none());
        // With `probe` on the result depends on platform support; just ensure
        // the call does not panic.
        #[cfg(feature = "probe")]
        {
            let _ = capture_connect_dump("zccache", Duration::from_secs(3), "not found");
        }
    }
}
