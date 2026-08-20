use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

use running_process_platform_internal::platform::terminal as pty_platform;

/// Native terminal input capture and translation helpers.
pub mod terminal_input;

/// Reports whether the process-wide ConPTY API table resolved to the
/// system `kernel32.dll` or to a sidecar `conpty.dll`. See #443.
///
/// Integration tests gate Win10-with-sidecar coverage on this — the
/// byte-exact passthrough assertions can only hold when a sidecar is
/// loaded on Win10, since the system `kernel32!CreatePseudoConsole`
/// on Win10 < build 22000 silently ignores `PSEUDOCONSOLE_PASSTHROUGH_MODE`.
pub use running_process_platform_internal::platform::terminal::{
    current_backend_kind, ConPtyBackendKind,
};

// #150: backend abstraction so native_pty_process.rs calls a single
// Backend::openpty() regardless of platform. Made `pub` in 4.0.1 so
// downstream consumers (e.g. clud's SIGWINCH relay) can call
// `PtyMaster::resize` / `get_size` through `NativePtyHandles.master`.
/// Cross-platform PTY backend traits and platform-selected implementations.
pub mod backend;
/// Re-exported PTY backend handles and size type.
pub use backend::{PtyChild, PtyMaster, PtySize};

/// Build an argv for the selected host shell.
pub fn platform_shell_argv(command: &str) -> Vec<String> {
    pty_platform::shell_argv(command)
}

/// Whether this host can observe child exit before closing the PTY master.
pub fn wait_before_close_supported() -> bool {
    pty_platform::wait_before_close_supported()
}

mod native_pty_process;
/// Re-exported native PTY process and interactive session types.
pub use native_pty_process::{
    InteractivePtyOptions, InteractivePtyPumpResult, InteractivePtySession, NativePtyProcess,
};

/// Async PTY facade using the bounded synchronous platform island.
#[cfg(feature = "async-process")]
pub mod async_pty;
#[cfg(feature = "async-process")]
pub use async_pty::{AsyncPtyProcess, IdleWaitOutcome};

/// Errors returned by pseudo-terminal process operations.
#[derive(Debug, Error)]
pub enum PtyError {
    /// The pseudo-terminal process has already been started.
    #[error("pseudo-terminal process already started")]
    AlreadyStarted,
    /// The pseudo-terminal process is not currently running.
    #[error("pseudo-terminal process is not running")]
    NotRunning,
    /// The pseudo-terminal operation exceeded its timeout.
    #[error("pseudo-terminal timed out")]
    Timeout,
    /// An underlying I/O operation failed.
    #[error("pseudo-terminal I/O error: {0}")]
    Io(
        /// The underlying I/O error.
        #[from]
        std::io::Error,
    ),
    /// Spawning the pseudo-terminal process failed.
    #[error("pseudo-terminal spawn failed: {0}")]
    Spawn(
        /// Backend-provided spawn failure details.
        String,
    ),
    /// A pseudo-terminal operation failed for another reason.
    #[error("pseudo-terminal error: {0}")]
    Other(
        /// Human-readable error details.
        String,
    ),
}

/// Return whether a process-control error can be ignored during cleanup.
pub fn is_ignorable_process_control_error(err: &std::io::Error) -> bool {
    pty_platform::is_ignorable_process_control_error(err)
}

/// Buffered output and close state for a PTY reader thread.
pub struct PtyReadState {
    /// Output chunks read from the PTY master.
    pub chunks: VecDeque<Vec<u8>>,
    /// Whether the PTY reader has reached EOF or stopped.
    pub closed: bool,
}

/// Shared reader state paired with a condition variable for waiters.
pub struct PtyReadShared {
    /// Protected reader buffer and close state.
    pub state: Mutex<PtyReadState>,
    /// Notifies waiters when output arrives or the reader closes.
    pub condvar: Condvar,
}

/// Platform-neutral handles for a running native PTY child.
/// Independently-lockable PTY input writer (issue #590, cluster D). Kept
/// separate from the `handles` mutex so a blocking write never holds that
/// lock — see the field docs on [`NativePtyHandles::writer`].
pub type SharedPtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

pub struct NativePtyHandles {
    // #150: master/child previously stored concrete backend types.
    // Refactored to use the cross-platform PtyMaster / PtyChild
    // traits so the Windows path goes through `conpty_passthrough`
    // (with PSEUDOCONSOLE_PASSTHROUGH_MODE) instead of portable-pty.
    /// Master side of the PTY, used for resize and size queries.
    pub master: Box<dyn crate::pty::backend::PtyMaster>,
    /// Writer connected to the PTY master input stream.
    ///
    /// Held in its own `Arc<Mutex<…>>` (issue #590, cluster D) so a
    /// blocking `write_all` on a full input pipe does NOT hold the outer
    /// `handles` mutex. Otherwise `close()`/`kill()`/`poll()` — which all
    /// lock `handles` — would deadlock behind an input write the child has
    /// stopped consuming.
    pub writer: SharedPtyWriter,
    /// Spawned child process attached to the PTY slave.
    pub child: Box<dyn crate::pty::backend::PtyChild>,
    /// Host-owned process-tree containment guard.
    pub process_guard: pty_platform::PtyProcessGuard,
}

/// Shared mutable state for idle detection waits.
pub struct IdleMonitorState {
    /// Last time input or qualifying output reset the idle timer.
    pub last_reset_at: Instant,
    /// Observed child return code, when the process has exited.
    pub returncode: Option<i32>,
    /// Whether the recorded exit was caused by an interrupt request.
    pub interrupted: bool,
}

/// Core idle detection logic, shareable across threads via Arc.
/// The reader thread calls `record_output` directly.
pub struct IdleDetectorCore {
    /// Minimum idle duration before the detector reports an idle timeout.
    pub timeout_seconds: f64,
    /// Additional quiet window required before reporting idle.
    pub stability_window_seconds: f64,
    /// Poll interval used while waiting for idle or exit.
    pub sample_interval_seconds: f64,
    /// Whether PTY input resets the idle timer.
    pub reset_on_input: bool,
    /// Whether PTY output resets the idle timer.
    pub reset_on_output: bool,
    /// Whether ANSI/control churn without visible bytes counts as output.
    pub count_control_churn_as_output: bool,
    /// Runtime switch that enables or disables idle timeout detection.
    pub enabled: Arc<AtomicBool>,
    /// Protected idle timing and exit state.
    pub state: Mutex<IdleMonitorState>,
    /// Notifies idle waiters when activity, exit, or enablement changes.
    pub condvar: Condvar,
}

impl IdleDetectorCore {
    /// Record input activity and reset the idle timer when configured.
    pub fn record_input(&self, byte_count: usize) {
        if !self.reset_on_input || byte_count == 0 {
            return;
        }
        let mut guard = self.state.lock().expect("idle monitor mutex poisoned");
        guard.last_reset_at = Instant::now();
        self.condvar.notify_all();
    }

    /// Record output activity and reset the idle timer when configured.
    pub fn record_output(&self, data: &[u8]) {
        if !self.reset_on_output || data.is_empty() {
            return;
        }
        let control_bytes = control_churn_bytes(data);
        let visible_output_bytes = data.len().saturating_sub(control_bytes);
        let active_output =
            visible_output_bytes > 0 || (self.count_control_churn_as_output && control_bytes > 0);
        if !active_output {
            return;
        }
        let mut guard = self.state.lock().expect("idle monitor mutex poisoned");
        guard.last_reset_at = Instant::now();
        self.condvar.notify_all();
    }

    /// Record child process exit information and wake idle waiters.
    pub fn mark_exit(&self, returncode: i32, interrupted: bool) {
        let mut guard = self.state.lock().expect("idle monitor mutex poisoned");
        guard.returncode = Some(returncode);
        guard.interrupted = interrupted;
        self.condvar.notify_all();
    }

    /// Return whether idle timeout detection is currently enabled.
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Enable or disable idle timeout detection.
    pub fn set_enabled(&self, enabled: bool) {
        let was_enabled = self.enabled.swap(enabled, Ordering::AcqRel);
        if enabled && !was_enabled {
            let mut guard = self.state.lock().expect("idle monitor mutex poisoned");
            guard.last_reset_at = Instant::now();
        }
        self.condvar.notify_all();
    }

    /// Wait until the child exits, the idle threshold is reached, or the timeout expires.
    pub fn wait(&self, timeout: Option<f64>) -> (bool, String, f64, Option<i32>) {
        let started = Instant::now();
        let overall_timeout = timeout.map(Duration::from_secs_f64);
        let min_idle = self.timeout_seconds.max(self.stability_window_seconds);
        let sample_interval = Duration::from_secs_f64(self.sample_interval_seconds.max(0.001));

        let mut guard = self.state.lock().expect("idle monitor mutex poisoned");
        loop {
            let now = Instant::now();
            let idle_for = now.duration_since(guard.last_reset_at).as_secs_f64();

            if let Some(returncode) = guard.returncode {
                let reason = if guard.interrupted {
                    "interrupt"
                } else {
                    "process_exit"
                };
                return (false, reason.to_string(), idle_for, Some(returncode));
            }

            let enabled = self.enabled.load(Ordering::Acquire);
            if enabled && idle_for >= min_idle {
                return (true, "idle_timeout".to_string(), idle_for, None);
            }

            if let Some(limit) = overall_timeout {
                if now.duration_since(started) >= limit {
                    return (false, "timeout".to_string(), idle_for, None);
                }
            }

            let idle_remaining = if enabled {
                (min_idle - idle_for).max(0.0)
            } else {
                sample_interval.as_secs_f64()
            };
            let mut wait_for =
                sample_interval.min(Duration::from_secs_f64(idle_remaining.max(0.001)));
            if let Some(limit) = overall_timeout {
                let elapsed = now.duration_since(started);
                if elapsed < limit {
                    let remaining = limit - elapsed;
                    wait_for = wait_for.min(remaining);
                }
            }
            let result = self
                .condvar
                .wait_timeout(guard, wait_for)
                .expect("idle monitor mutex poisoned");
            guard = result.0;
        }
    }
}

// ── Helper functions ──

/// Count ANSI/control bytes that should not be treated as visible output.
pub fn control_churn_bytes(data: &[u8]) -> usize {
    let mut total = 0;
    let mut index = 0;
    while index < data.len() {
        let byte = data[index];
        if byte == 0x1B {
            let start = index;
            index += 1;
            if index < data.len() && data[index] == b'[' {
                index += 1;
                while index < data.len() {
                    let current = data[index];
                    index += 1;
                    if (0x40..=0x7E).contains(&current) {
                        break;
                    }
                }
            }
            total += index - start;
            continue;
        }
        if matches!(byte, 0x08 | 0x0D | 0x7F) {
            total += 1;
        }
        index += 1;
    }
    total
}

/// Spawn the background reader that drains PTY output into shared state.
#[inline(never)]
pub fn spawn_pty_reader(
    mut reader: Box<dyn Read + Send>,
    shared: Arc<PtyReadShared>,
    echo: Arc<AtomicBool>,
    idle_detector: Arc<Mutex<Option<Arc<IdleDetectorCore>>>>,
    output_bytes_total: Arc<AtomicUsize>,
    control_churn_bytes_total: Arc<AtomicUsize>,
) {
    crate::rp_rust_debug_scope!("running_process::spawn_pty_reader");
    let idle_detector_snapshot = idle_detector
        .lock()
        .expect("idle detector mutex poisoned")
        .clone();
    let mut chunk = vec![0_u8; 65536];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let data = &chunk[..n];

                let churn = control_churn_bytes(data);
                let visible = data.len().saturating_sub(churn);
                output_bytes_total.fetch_add(visible, Ordering::Relaxed);
                control_churn_bytes_total.fetch_add(churn, Ordering::Relaxed);

                if echo.load(Ordering::Relaxed) {
                    let _ = std::io::stdout().write_all(data);
                    let _ = std::io::stdout().flush();
                }

                if let Some(ref detector) = idle_detector_snapshot {
                    detector.record_output(data);
                }

                let mut guard = shared.state.lock().expect("pty read mutex poisoned");
                guard.chunks.push_back(data.to_vec());
                shared.condvar.notify_all();
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                // #199: intentional — back-off on a non-blocking PTY
                // master read that returned WouldBlock. There's no
                // POSIX "wait for fd readable" that's portable
                // across the OwnedFd / Windows OwnedHandle paths
                // used here.
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(_) => break,
        }
    }
    let mut guard = shared.state.lock().expect("pty read mutex poisoned");
    guard.closed = true;
    shared.condvar.notify_all();
}

/// Return whether input bytes contain a carriage return or newline.
pub fn input_contains_newline(data: &[u8]) -> bool {
    data.iter().any(|byte| matches!(*byte, b'\r' | b'\n'))
}

/// Relay bytes from the selected host terminal into the active PTY until stopped or exited.
pub(super) struct TerminalInputRelayState {
    pub handles: Arc<Mutex<Option<NativePtyHandles>>>,
    pub returncode: Arc<Mutex<Option<i32>>>,
    pub input_bytes_total: Arc<AtomicUsize>,
    pub newline_events_total: Arc<AtomicUsize>,
    pub submit_events_total: Arc<AtomicUsize>,
    pub stop: Arc<AtomicBool>,
    pub active: Arc<AtomicBool>,
}

#[inline(never)]
pub(super) fn terminal_input_relay_worker(
    input: pty_platform::TerminalInputSession,
    state: TerminalInputRelayState,
) {
    loop {
        if state.stop.load(Ordering::Acquire) {
            break;
        }
        match poll_pty_process(&state.handles, &state.returncode) {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => break,
        }

        let chunk = match input.read_chunk(Duration::from_millis(50)) {
            Ok(Some(chunk)) => chunk,
            Ok(None) => continue,
            Err(_) => break,
        };

        record_pty_input_metrics(
            &state.input_bytes_total,
            &state.newline_events_total,
            &state.submit_events_total,
            &chunk.data,
            chunk.submit,
        );
        if write_pty_input(&state.handles, &chunk.data).is_err() {
            break;
        }
    }

    state.active.store(false, Ordering::Release);
}

/// Record PTY input byte, newline, and submit counters for one input chunk.
pub fn record_pty_input_metrics(
    input_bytes_total: &Arc<AtomicUsize>,
    newline_events_total: &Arc<AtomicUsize>,
    submit_events_total: &Arc<AtomicUsize>,
    data: &[u8],
    submit: bool,
) {
    input_bytes_total.fetch_add(data.len(), Ordering::AcqRel);
    if input_contains_newline(data) {
        newline_events_total.fetch_add(1, Ordering::AcqRel);
    }
    if submit {
        submit_events_total.fetch_add(1, Ordering::AcqRel);
    }
}

/// Store the PTY child return code in shared process state.
pub fn store_pty_returncode(returncode: &Arc<Mutex<Option<i32>>>, code: i32) {
    *returncode.lock().expect("pty returncode mutex poisoned") = Some(code);
}

/// Poll the PTY child process and persist its return code after exit.
pub fn poll_pty_process(
    handles: &Arc<Mutex<Option<NativePtyHandles>>>,
    returncode: &Arc<Mutex<Option<i32>>>,
) -> Result<Option<i32>, std::io::Error> {
    let mut guard = handles.lock().expect("pty handles mutex poisoned");
    let Some(handles) = guard.as_mut() else {
        return Ok(*returncode.lock().expect("pty returncode mutex poisoned"));
    };
    let status = handles.child.try_wait()?;
    // #150: try_wait now returns Option<u32> (from PtyChild trait)
    // The platform-owned child status is an unsigned exit code. Cast for storage.
    let code = status.map(|c| c as i32);
    if let Some(code) = code {
        store_pty_returncode(returncode, code);
        return Ok(Some(code));
    }
    Ok(None)
}

/// Write input bytes to the running PTY after platform-specific translation.
pub fn write_pty_input(
    handles: &Arc<Mutex<Option<NativePtyHandles>>>,
    data: &[u8],
) -> Result<(), std::io::Error> {
    // Clone the writer handle out from under the `handles` lock, then
    // release `handles` BEFORE the blocking write (issue #590, cluster D).
    // The PTY input pipe fills when the child stops reading stdin; a
    // `write_all` that blocked while holding `handles` would deadlock every
    // teardown/poll path that also locks `handles`.
    let writer = {
        let guard = handles.lock().expect("pty handles mutex poisoned");
        let handles = guard.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "Pseudo-terminal process is not running",
            )
        })?;
        Arc::clone(&handles.writer)
    };
    let payload = pty_platform::input_payload(data);
    let mut writer = writer.lock().expect("pty writer mutex poisoned");
    writer.write_all(&payload)?;
    writer.flush()
}

/// Translate newline bytes into the Windows PTY input payload format.
pub fn windows_terminal_input_payload(data: &[u8]) -> Vec<u8> {
    pty_platform::input_payload(data)
}

/// Compatibility name for the host-owned PTY process-tree guard.
pub type WindowsJobHandle = pty_platform::PtyProcessGuard;

/// Information about a child process found via Toolhelp snapshot.
pub use pty_platform::ChildProcessInfo;

/// Find all direct child processes of a given parent PID using the Windows Toolhelp API.
/// Returns PID and process name for each child.
pub fn find_child_processes(parent_pid: u32) -> Vec<ChildProcessInfo> {
    pty_platform::find_child_processes(parent_pid)
}

/// A conhost.exe process whose parent is no longer alive — likely an orphan
/// from a dead ConPTY session.
pub use pty_platform::OrphanConhostInfo;

/// Scan all conhost.exe processes on the system and return those whose parent
/// process is no longer alive. These are likely orphans from dead ConPTY sessions.
///
/// Uses `CreateToolhelp32Snapshot` for a point-in-time snapshot — no sysinfo
/// dependency, so it's lightweight and can be called frequently.
pub fn find_orphan_conhosts() -> Vec<OrphanConhostInfo> {
    pty_platform::find_orphan_conhosts()
}

#[cfg(test)]
mod tests {
    use super::native_pty_process::resolved_spawn_cwd;

    #[test]
    fn resolved_spawn_cwd_preserves_explicit_value() {
        assert_eq!(
            resolved_spawn_cwd(Some("C:\\temp\\explicit")),
            Some("C:\\temp\\explicit".to_string())
        );
    }

    #[test]
    fn resolved_spawn_cwd_defaults_to_current_dir_when_unset() {
        let expected = std::env::current_dir()
            .ok()
            .map(|cwd| cwd.to_string_lossy().to_string());
        assert_eq!(resolved_spawn_cwd(None), expected);
    }
}

#[cfg(test)]
#[path = "../tests/pty_core_coverage.rs"]
mod coverage_tests;
