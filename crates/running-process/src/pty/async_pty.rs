//! Async PTY facade over the bounded synchronous PTY boundary.
//!
//! PTY backends still expose synchronous operations on some supported
//! platforms (notably ConPTY). This module keeps those calls off Tokio workers
//! behind `crate::blocking_island`, the single process-wide bounded island
//! shared with the other operations that have no async form. The process actor
//! remains the canonical async engine for pipes; this boundary is intentionally
//! narrow, observable, and temporary until native PTY readiness is available.
//!
//! Not every operation here is dispatched. Metric getters, echo state, and the
//! relay-stop request touch only atomics, so they stay synchronous: routing a
//! load of an `AtomicUsize` through a blocking worker would cost a permit and
//! buy nothing.

use std::sync::Arc;
use std::time::Duration;

use super::{IdleDetectorCore, NativePtyProcess, PtyError};
use crate::blocking_island::dispatch;

/// Result of an async idle wait.
///
/// The sync detector returns a bare 4-tuple. Naming the fields here keeps the
/// async surface readable without changing the sync return type, which is part
/// of the frozen compatibility surface.
#[derive(Clone, Debug, PartialEq)]
pub struct IdleWaitOutcome {
    /// Whether the idle condition was reached before the timeout.
    pub reached: bool,
    /// Human-readable reason the wait ended.
    pub reason: String,
    /// Seconds the PTY was idle when the wait ended.
    pub idle_seconds: f64,
    /// Child exit code, if the child exited during the wait.
    pub returncode: Option<i32>,
}

/// Dispatch a fallible PTY call onto the shared bounded island.
async fn run_blocking<T, F>(operation: F) -> Result<T, PtyError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, PtyError> + Send + 'static,
{
    infallible(operation).await?
}

/// Dispatch a PTY call that cannot itself fail.
///
/// The `Result` that remains is the island's own failure to run the call, not
/// the call's outcome.
async fn infallible<T, F>(operation: F) -> Result<T, PtyError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    dispatch(operation)
        .await
        .map_err(|error| PtyError::Other(format!("PTY blocking operation failed: {error}")))
}

/// Async PTY handle backed by the bounded synchronous PTY island.
///
/// Every operation is dispatched through at most two blocking workers shared
/// by all handles. Dropping an async future stops waiting for its result, but
/// cannot interrupt an OS PTY call already executing; the bounded worker
/// remains responsible for releasing its permit and completing teardown.
#[derive(Clone)]
pub struct AsyncPtyProcess {
    process: Arc<NativePtyProcess>,
}

impl AsyncPtyProcess {
    /// Construct an async PTY process from the existing PTY configuration.
    pub fn new(
        argv: Vec<String>,
        cwd: Option<String>,
        env: Option<Vec<(String, String)>>,
        rows: u16,
        cols: u16,
        nice: Option<i32>,
    ) -> Result<Self, PtyError> {
        Ok(Self {
            process: Arc::new(NativePtyProcess::new(argv, cwd, env, rows, cols, nice)?),
        })
    }

    /// Start the PTY child without blocking a Tokio worker.
    pub async fn start(&self) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.start_impl()).await
    }

    /// Read one PTY output chunk. `None` means the bounded read timed out.
    pub async fn read_chunk(&self, timeout: Option<Duration>) -> Result<Option<Vec<u8>>, PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.read_chunk_impl(timeout.map(|value| value.as_secs_f64())))
            .await
    }

    /// Write bytes to the PTY input stream.
    pub async fn write(&self, bytes: Vec<u8>, submit: bool) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.write_impl(&bytes, submit)).await
    }

    /// Resize the PTY.
    pub async fn resize(&self, rows: u16, cols: u16) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.resize_impl(rows, cols)).await
    }

    /// Wait for the child and return its exit code.
    pub async fn wait(&self, timeout: Option<Duration>) -> Result<i32, PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.wait_impl(timeout.map(|value| value.as_secs_f64()))).await
    }

    /// Request graceful termination of the PTY child.
    pub async fn terminate(&self) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.terminate_impl()).await
    }

    /// Forcefully terminate the PTY child.
    pub async fn kill(&self) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.kill_impl()).await
    }

    /// Close the PTY and complete bounded teardown.
    pub async fn close(&self) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.close_impl()).await
    }

    /// Return the child PID, if the PTY has started.
    pub async fn pid(&self) -> Result<Option<u32>, PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.pid()).await
    }

    /// Deliver an interrupt (Ctrl+C / SIGINT) to the PTY child.
    pub async fn send_interrupt(&self) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.send_interrupt_impl()).await
    }

    /// Gracefully terminate the PTY child and its descendants.
    pub async fn terminate_tree(&self) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.terminate_tree_impl()).await
    }

    /// Forcefully kill the PTY child and its descendants.
    pub async fn kill_tree(&self) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.kill_tree_impl()).await
    }

    /// Reply to terminal capability queries the child emitted.
    pub async fn respond_to_queries(&self, data: Vec<u8>) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.respond_to_queries_impl(&data)).await
    }

    /// Wait for the child, then drain output that is still in flight.
    ///
    /// Exit and EOF are separate events on a PTY: the child can exit while the
    /// master still holds buffered output. This waits for both, which is why a
    /// plain [`Self::wait`] can leave bytes unread.
    pub async fn wait_and_drain(
        &self,
        timeout: Option<Duration>,
        drain_timeout: Duration,
    ) -> Result<i32, PtyError> {
        let process = Arc::clone(&self.process);
        let timeout = timeout.map(|value| value.as_secs_f64());
        let drain_timeout = drain_timeout.as_secs_f64();
        run_blocking(move || process.wait_and_drain_impl(timeout, drain_timeout)).await
    }

    /// Wait until the PTY reader side has closed. `false` means it timed out.
    pub async fn wait_for_reader_closed(
        &self,
        timeout: Option<Duration>,
    ) -> Result<bool, PtyError> {
        let process = Arc::clone(&self.process);
        let timeout = timeout.map(|value| value.as_secs_f64());
        infallible(move || process.wait_for_reader_closed_impl(timeout)).await
    }

    /// Attach an idle detector so PTY traffic feeds its idle clock.
    pub async fn attach_idle_detector(
        &self,
        detector: Arc<IdleDetectorCore>,
    ) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        infallible(move || process.attach_idle_detector(&detector)).await
    }

    /// Detach the currently attached idle detector.
    pub async fn detach_idle_detector(&self) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        infallible(move || process.detach_idle_detector()).await
    }

    /// Wait until the attached detector reports the PTY idle.
    ///
    /// The detector is supplied by the caller because idle policy belongs to
    /// the caller, not the PTY: what counts as idle differs between "the shell
    /// stopped printing" and "the build finished".
    pub async fn wait_for_idle(
        &self,
        detector: Arc<IdleDetectorCore>,
        timeout: Option<Duration>,
    ) -> Result<IdleWaitOutcome, PtyError> {
        let timeout = timeout.map(|value| value.as_secs_f64());
        infallible(move || {
            let (reached, reason, idle_seconds, returncode) = detector.wait(timeout);
            IdleWaitOutcome {
                reached,
                reason,
                idle_seconds,
                returncode,
            }
        })
        .await
    }

    /// Start relaying host terminal input into the PTY.
    pub async fn start_terminal_input_relay(&self) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.start_terminal_input_relay_impl()).await
    }

    /// Stop the terminal input relay and wait for its worker to finish.
    pub async fn stop_terminal_input_relay(&self) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        infallible(move || process.stop_terminal_input_relay_impl()).await
    }

    /// Ask the terminal input relay to stop without waiting for it.
    ///
    /// Synchronous and non-blocking by design: this is what a signal handler
    /// or a `Drop` can call, neither of which can await.
    pub fn request_terminal_input_relay_stop(&self) {
        self.process.request_terminal_input_relay_stop();
    }

    /// Whether the terminal input relay is currently running.
    pub fn terminal_input_relay_active(&self) -> bool {
        self.process.terminal_input_relay_active()
    }

    /// Set whether PTY input is echoed back to the host.
    pub fn set_echo(&self, enabled: bool) {
        self.process.set_echo(enabled);
    }

    /// Whether PTY input echo is enabled.
    pub fn echo_enabled(&self) -> bool {
        self.process.echo_enabled()
    }

    /// Close the PTY without waiting for bounded teardown to finish.
    ///
    /// Synchronous for the same reason as
    /// [`Self::request_terminal_input_relay_stop`]: it exists for teardown
    /// paths that cannot await.
    pub fn close_nonblocking(&self) {
        self.process.close_nonblocking();
    }

    /// Record that the PTY reader has closed.
    pub fn mark_reader_closed(&self) {
        self.process.mark_reader_closed();
    }

    /// Record the child exit code observed out of band.
    pub fn store_returncode(&self, code: i32) {
        self.process.store_returncode(code);
    }

    /// Account for bytes written to the PTY by a path outside this handle.
    pub fn record_input_metrics(&self, data: &[u8], submit: bool) {
        self.process.record_input_metrics(data, submit);
    }

    /// Total bytes written to the PTY input stream.
    pub fn pty_input_bytes_total(&self) -> usize {
        self.process.pty_input_bytes_total()
    }

    /// Total newline events observed on PTY input.
    pub fn pty_newline_events_total(&self) -> usize {
        self.process.pty_newline_events_total()
    }

    /// Total submit events observed on PTY input.
    pub fn pty_submit_events_total(&self) -> usize {
        self.process.pty_submit_events_total()
    }

    /// Total bytes read from the PTY output stream.
    pub fn pty_output_bytes_total(&self) -> usize {
        self.process.pty_output_bytes_total()
    }

    /// Total terminal-control bytes seen in PTY output.
    pub fn pty_control_churn_bytes_total(&self) -> usize {
        self.process.pty_control_churn_bytes_total()
    }
}

#[cfg(test)]
mod tests {
    use super::AsyncPtyProcess;
    use std::time::Duration;

    #[tokio::test]
    async fn async_pty_dispatches_start_read_and_close_through_island() {
        #[cfg(windows)]
        let argv = vec!["cmd.exe".into(), "/C".into(), "echo async-pty".into()];
        #[cfg(not(windows))]
        let argv = vec!["/bin/sh".into(), "-c".into(), "printf async-pty".into()];

        let process =
            AsyncPtyProcess::new(argv, None, None, 24, 80, None).expect("async PTY configuration");
        process.start().await.expect("async PTY start");
        let _ = process.read_chunk(Some(Duration::from_secs(1))).await;
        assert!(process.pid().await.expect("async PTY pid").is_some());
        process.close().await.expect("async PTY close");
    }
}
