//! Spawning and supervising the symbolization worker (#637).
//!
//! The daemon never parses a symbol file. It hands a capture to a short-lived
//! child, reads a report back, and treats anything else as a degraded
//! symbolization. That the child is a separate *process* is the whole point:
//! a PDB or minidump can be malformed in ways that crash a parser outright
//! rather than returning an error, and a crash cannot be caught in-process.
//!
//! # Every failure is degraded, never fatal
//!
//! A missing worker binary, a crash, a timeout, unreadable output — all
//! produce a [`WorkerError`] the caller reports alongside the raw capture.
//! The daemon is long-lived and shared; losing it because one capture was
//! unsymbolizable would take every other registrant's diagnostics with it.
//!
//! # Deadline
//!
//! The child is bounded by wall-clock time and killed if it overruns. A
//! symbolization worker that hangs — waiting on a network symbol server, or
//! looping on a crafted input — must not pin a daemon thread indefinitely.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use running_process::spawn::{SpawnStdio, StdioSource};

/// Environment variable naming an explicit worker binary.
///
/// Tests point this at a build artifact; deployments rely on the sibling
/// lookup, since the daemon and worker ship together.
pub const WORKER_PATH_ENV: &str = "RUNNING_PROCESS_PROBE_WORKER";

/// How long a worker may run before it is killed.
pub const DEFAULT_WORKER_TIMEOUT: Duration = Duration::from_secs(60);

/// Largest report accepted back from a worker.
///
/// The worker is ours, but it parses hostile input, so its output is treated
/// as untrusted too — a compromised or confused worker must not be able to
/// exhaust the daemon's memory through its stdout.
pub const MAX_REPORT_BYTES: u64 = 64 * 1024 * 1024;

/// Why symbolization did not produce a report.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// The worker binary could not be located.
    #[error("symbolization worker not found (set {WORKER_PATH_ENV} to override)")]
    NotFound,
    /// The worker could not be started.
    #[error("cannot start symbolization worker: {0}")]
    Spawn(#[source] std::io::Error),
    /// Talking to the worker failed.
    #[error("symbolization worker I/O failed: {0}")]
    Io(#[source] std::io::Error),
    /// The worker exited non-zero — including by crashing, which is the
    /// isolation contract working as designed.
    #[error("symbolization worker exited with code {code}: {stderr}")]
    WorkerDied {
        /// Exit status reported by the OS.
        code: i32,
        /// Whatever the worker managed to say before dying.
        stderr: String,
    },
    /// The worker outlived its deadline and was killed.
    #[error("symbolization worker exceeded its {0:?} deadline and was killed")]
    Timeout(Duration),
    /// The worker's output was not a report.
    #[error("symbolization worker produced unreadable output: {0}")]
    BadReport(String),
}

/// Locate the worker binary.
///
/// The override wins; otherwise it is looked for beside the running
/// executable, because the daemon and worker are built and shipped together.
pub fn worker_path() -> Option<PathBuf> {
    resolve_worker_path(std::env::var_os(WORKER_PATH_ENV))
}

/// Resolution logic for [`worker_path`], with the override supplied directly.
///
/// Taking the override as an argument keeps the decision testable without
/// mutating process-global environment state, which would otherwise make the
/// tests order-dependent on each other.
pub fn resolve_worker_path(explicit: Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(explicit) = explicit {
        let path = PathBuf::from(explicit);
        // An override naming a missing file resolves to nothing rather than
        // falling back: silently symbolizing with a different binary than the
        // operator named would make the override untrustworthy.
        return path.is_file().then_some(path);
    }
    let exe = std::env::current_exe().ok()?;
    let sibling = exe.parent()?.join(format!(
        "running-process-probe-worker{}",
        std::env::consts::EXE_SUFFIX
    ));
    sibling.is_file().then_some(sibling)
}

/// Hand `capture_json` to a worker and read back its report.
///
/// `capture_json` is passed through opaquely: the daemon deliberately does not
/// model the capture schema beyond what it needs to route it, so a schema
/// change does not require a daemon release.
pub fn symbolize_with_worker(
    capture_json: &[u8],
    timeout: Duration,
) -> Result<String, WorkerError> {
    let binary = worker_path().ok_or(WorkerError::NotFound)?;
    symbolize_with_worker_at(&binary, capture_json, timeout)
}

/// Like [`symbolize_with_worker`] but against an explicit binary.
pub fn symbolize_with_worker_at(
    binary: &Path,
    capture_json: &[u8],
    timeout: Duration,
) -> Result<String, WorkerError> {
    // Routed through the sanitized spawn layer so the child gets sanitized
    // handles and no visible console, like every other spawn in the workspace.
    let mut command = std::process::Command::new(binary);
    let mut child = running_process::spawn::spawn(
        &mut command,
        SpawnStdio {
            stdin: StdioSource::Pipe,
            stdout: StdioSource::Pipe,
            stderr: StdioSource::Pipe,
            ..Default::default()
        },
    )
    .map_err(WorkerError::Spawn)?;

    // Write the capture and close stdin. Closing is what tells the worker the
    // capture is complete; without it both sides wait for the other.
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| WorkerError::Io(std::io::Error::other("worker stdin was not piped")))?;
        // A worker that dies before reading breaks the pipe. That is not an
        // I/O bug on our side — the exit status below is the real diagnosis,
        // so record the write failure and keep going.
        let _ = stdin.write_all(capture_json);
        let _ = stdin.flush();
    }

    // Drain stdout and stderr on threads. Reading them in sequence would
    // deadlock as soon as the worker filled the pipe we were not reading.
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(handle) = stdout.as_mut() {
            let _ = handle.take(MAX_REPORT_BYTES).read_to_end(&mut buffer);
        }
        buffer
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(handle) = stderr.as_mut() {
            // Bounded too: stderr is diagnostic text, and a worker looping on
            // an error message should not grow the daemon's memory.
            let _ = handle.take(MAX_REPORT_BYTES).read_to_end(&mut buffer);
        }
        buffer
    });

    let deadline = Instant::now() + timeout;
    let code = loop {
        match child.try_wait() {
            Ok(Some(code)) => break code,
            Ok(None) => {}
            Err(e) => return Err(WorkerError::Io(e)),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            // Reap so the killed child does not linger as a zombie.
            let _ = child.wait();
            return Err(WorkerError::Timeout(timeout));
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let stdout_bytes = stdout_reader.join().unwrap_or_default();
    let stderr_text = String::from_utf8_lossy(&stderr_reader.join().unwrap_or_default())
        .trim()
        .to_string();

    if code != 0 {
        return Err(WorkerError::WorkerDied {
            code,
            stderr: stderr_text,
        });
    }

    let report = String::from_utf8(stdout_bytes)
        .map_err(|e| WorkerError::BadReport(format!("report was not UTF-8: {e}")))?;
    if report.trim().is_empty() {
        return Err(WorkerError::BadReport(
            "worker exited successfully but wrote no report".into(),
        ));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPTURE: &str = r#"{"format":"cooperative_frames","modules":[{"name":"m.dll"}],
        "threads":[{"os_tid":7,"frames":[{"module_index":0,"relative_address":16}]}]}"#;

    /// Path to the worker binary built alongside these tests.
    ///
    /// Returns `None` on a targeted local run that built only this crate. A
    /// workspace run — which is what CI and `./test` do — always produces it,
    /// so a miss *there* means the tests below would skip silently and prove
    /// nothing. That is failed loudly rather than tolerated.
    fn worker_binary() -> Option<PathBuf> {
        let mut path = std::env::current_exe().ok()?;
        path.pop();
        if path.ends_with("deps") {
            path.pop();
        }
        let candidate = path.join(format!(
            "running-process-probe-worker{}",
            std::env::consts::EXE_SUFFIX
        ));
        if candidate.is_file() {
            return Some(candidate);
        }
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "worker binary missing at {} during a CI run; these tests would \
             skip and assert nothing",
            candidate.display()
        );
        None
    }

    #[test]
    fn a_capture_round_trips_through_a_real_worker() {
        let Some(binary) = worker_binary() else {
            // The worker is a separate crate; a filtered build may not have
            // produced it. Skipping is honest — asserting nothing would not be.
            eprintln!("skipping: worker binary not built");
            return;
        };

        let report = symbolize_with_worker_at(&binary, CAPTURE.as_bytes(), DEFAULT_WORKER_TIMEOUT)
            .expect("worker should symbolize a well-formed capture");
        assert!(
            report.contains("m.dll"),
            "report should name the module; got {report}"
        );
    }

    /// The isolation contract, observed from the daemon's side.
    #[test]
    fn a_worker_that_rejects_its_input_is_reported_not_propagated() {
        let Some(binary) = worker_binary() else {
            eprintln!("skipping: worker binary not built");
            return;
        };

        let error = symbolize_with_worker_at(&binary, &[0xFF; 4096], DEFAULT_WORKER_TIMEOUT)
            .expect_err("garbage must not produce a report");
        match error {
            WorkerError::WorkerDied { code, stderr } => {
                assert_ne!(code, 0);
                assert!(!stderr.is_empty(), "the worker should say why it failed");
            }
            other => panic!("expected WorkerDied, got {other}"),
        }

        // The daemon is unaffected and can symbolize immediately afterwards.
        let report = symbolize_with_worker_at(&binary, CAPTURE.as_bytes(), DEFAULT_WORKER_TIMEOUT)
            .expect("a prior failure must not affect later work");
        assert!(report.contains("m.dll"));
    }

    #[test]
    fn a_missing_binary_is_not_found_rather_than_a_panic() {
        let missing = PathBuf::from("definitely-not-a-real-worker-binary");
        let error = symbolize_with_worker_at(&missing, CAPTURE.as_bytes(), DEFAULT_WORKER_TIMEOUT)
            .expect_err("a missing binary cannot symbolize");
        assert!(
            matches!(error, WorkerError::Spawn(_)),
            "expected a spawn failure, got {error}"
        );
    }

    #[test]
    fn the_override_wins_when_it_names_a_file() {
        let Some(binary) = worker_binary() else {
            eprintln!("skipping: worker binary not built");
            return;
        };
        let resolved = resolve_worker_path(Some(binary.clone().into_os_string()));
        assert_eq!(resolved.as_deref(), Some(binary.as_path()));
    }

    /// An override naming a missing file must resolve to nothing, not fall
    /// back — silently using a different binary than the operator named would
    /// make the override untrustworthy.
    #[test]
    fn an_override_naming_a_nonexistent_file_resolves_to_nothing() {
        let resolved = resolve_worker_path(Some("no-such-worker-binary-anywhere".into()));
        assert_eq!(resolved, None);
    }
}
