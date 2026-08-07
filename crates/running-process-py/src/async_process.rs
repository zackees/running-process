//! Native Python awaitables backed by the Rust async process and PTY APIs.

use std::ffi::OsString;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use pyo3_async_runtimes::tokio::future_into_py;
use running_process::pty::AsyncPtyProcess;
use running_process::{AsyncProcess, CursorRead, RunOutput, SharedOutputCursor, StreamKind};
use tokio::sync::{Mutex, RwLock};

fn process_error(error: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

/// Python awaitable process facade backed by native Rust futures.
///
/// An `RwLock`, not a `Mutex`. Only `start` and `close` mutate the handle;
/// everything else forwards a command to the actor and needs shared access
/// only. Under a `Mutex` an in-flight `output()` held the handle for its whole
/// duration, so `kill()` could never run during a capture -- which is exactly
/// the concurrency the actor exists to provide, and the sync surface has
/// always had. It deadlocked in practice, not just in theory.
#[pyclass(module = "running_process._native")]
pub(crate) struct AsyncRunningProcess {
    process: Arc<RwLock<AsyncProcess>>,
}

#[pymethods]
impl AsyncRunningProcess {
    #[new]
    #[pyo3(signature = (program, args, create_process_group=false))]
    fn new(program: String, args: Vec<String>, create_process_group: bool) -> Self {
        let mut process = AsyncProcess::new(OsString::from(program));
        for arg in args {
            process = process.arg(OsString::from(arg));
        }
        // Defaults to off: an owned group also detaches the child from the
        // parent's console Ctrl+C, which is not what most callers want.
        process = process.create_process_group(create_process_group);
        Self {
            process: Arc::new(RwLock::new(process)),
        }
    }

    /// Start the configured child and return when the actor owns it.
    fn start<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            process.write().await.start().await.map_err(process_error)
        })
    }

    /// Return the child pid.
    fn pid<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            process.read().await.pid().await.map_err(process_error)
        })
    }

    /// Wait for exit and return the platform-normalized exit code.
    fn wait<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            process
                .read()
                .await
                .wait()
                .await
                .map(exit_code)
                .map_err(process_error)
        })
    }

    /// Wait for exit up to `timeout` seconds.
    #[pyo3(signature = (timeout))]
    fn wait_timeout<'py>(&self, py: Python<'py>, timeout: f64) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            process
                .read()
                .await
                .wait_timeout(Duration::from_secs_f64(timeout))
                .await
                .map(exit_code)
                .map_err(process_error)
        })
    }

    /// Terminate the child.
    fn kill<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            process.read().await.kill().await.map_err(process_error)
        })
    }

    /// Write bytes to stdin.
    fn write_stdin<'py>(&self, py: Python<'py>, bytes: Vec<u8>) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            process
                .read()
                .await
                .write_stdin(bytes)
                .await
                .map_err(process_error)
        })
    }

    /// Close stdin and deliver EOF.
    fn close_stdin<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            process
                .read()
                .await
                .close_stdin()
                .await
                .map_err(process_error)
        })
    }

    /// Capture output after starting the child.
    fn output<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            process
                .read()
                .await
                .output()
                .await
                .map(output_tuple)
                .map_err(process_error)
        })
    }

    /// Capture output with an aggregate byte bound.
    fn output_bounded<'py>(&self, py: Python<'py>, limit: usize) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            process
                .read()
                .await
                .output_bounded(limit)
                .await
                .map(output_tuple)
                .map_err(process_error)
        })
    }

    /// Report the exit code if the child has already exited, without waiting.
    fn poll<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            process
                .read()
                .await
                .poll()
                .await
                .map(|status| status.map(exit_code))
                .map_err(process_error)
        })
    }

    /// Report the exit code if the child has already exited.
    fn returncode<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            process
                .read()
                .await
                .returncode()
                .await
                .map_err(process_error)
        })
    }

    /// Terminate the child. The sync surface spells this `terminate`; keeping
    /// both spellings means a caller porting from it does not have to rename.
    fn terminate<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            process
                .read()
                .await
                .terminate()
                .await
                .map_err(process_error)
        })
    }

    /// Release the actor and its child handles without killing the child.
    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            process.write().await.close().await.map_err(process_error)
        })
    }

    /// Ask the child's process group to shut down gracefully.
    ///
    /// Returns `False` when the child owns no group, so there was nothing
    /// addressable to signal.
    fn terminate_group_soft<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            process
                .read()
                .await
                .terminate_group_soft()
                .await
                .map_err(process_error)
        })
    }

    /// Kill the child and every descendant it has right now.
    #[pyo3(signature = (timeout=5.0))]
    fn kill_tree<'py>(&self, py: Python<'py>, timeout: f64) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            process
                .read()
                .await
                .kill_tree(Duration::from_secs_f64(timeout))
                .await
                .map_err(process_error)
        })
    }

    /// Open an independent cursor over the output the actor has retained.
    ///
    /// Synchronous on purpose: opening a cursor only clones a handle. The
    /// awaiting happens on the cursor's own `read_next`.
    fn output_cursor(&self) -> PyResult<AsyncOutputCursor> {
        // `try_lock` rather than a blocking lock: this is called from the
        // Python thread while the GIL is held, and blocking there on a lock an
        // actor future owns would deadlock the interpreter.
        let process = self
            .process
            .try_read()
            .map_err(|_| PyRuntimeError::new_err("process is busy in an exclusive operation"))?;
        let cursor = process.output_cursor().map_err(process_error)?;
        Ok(AsyncOutputCursor {
            cursor: Arc::new(Mutex::new(cursor)),
        })
    }

    /// Spawn, wait, and return `(exit_code, stdout_bytes, stderr_bytes)`.
    fn run<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            let mut process = process.write().await;
            process.start().await.map_err(process_error)?;
            process
                .output()
                .await
                .map(output_tuple)
                .map_err(process_error)
        })
    }
}

/// Python awaitable PTY facade backed by the bounded Rust PTY island.
#[pyclass(module = "running_process._native")]
pub(crate) struct AsyncPseudoTerminalProcess {
    process: AsyncPtyProcess,
}

#[pymethods]
impl AsyncPseudoTerminalProcess {
    #[new]
    #[pyo3(signature = (argv, cwd=None, env=None, rows=24, cols=80, nice=None))]
    fn new(
        argv: Vec<String>,
        cwd: Option<String>,
        env: Option<Bound<'_, PyDict>>,
        rows: u16,
        cols: u16,
        nice: Option<i32>,
    ) -> PyResult<Self> {
        let env_pairs = env
            .map(|mapping| {
                mapping
                    .iter()
                    .map(|(key, value)| Ok((key.extract::<String>()?, value.extract::<String>()?)))
                    .collect::<PyResult<Vec<(String, String)>>>()
            })
            .transpose()?;
        let process =
            AsyncPtyProcess::new(argv, cwd, env_pairs, rows, cols, nice).map_err(process_error)?;
        Ok(Self { process })
    }

    /// Return an awaitable that starts the PTY child.
    fn start<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = self.process.clone();
        future_into_py(
            py,
            async move { process.start().await.map_err(process_error) },
        )
    }

    /// Return an awaitable yielding the next PTY output chunk or `None`.
    #[pyo3(signature = (timeout=None))]
    fn read<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let process = self.process.clone();
        future_into_py(py, async move {
            process
                .read_chunk(timeout.map(Duration::from_secs_f64))
                .await
                .map_err(process_error)
        })
    }

    /// Return an awaitable that closes the PTY.
    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = self.process.clone();
        future_into_py(
            py,
            async move { process.close().await.map_err(process_error) },
        )
    }

    fn write<'py>(
        &self,
        py: Python<'py>,
        bytes: Vec<u8>,
        submit: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let process = self.process.clone();
        future_into_py(py, async move {
            process.write(bytes, submit).await.map_err(process_error)
        })
    }

    fn resize<'py>(&self, py: Python<'py>, rows: u16, cols: u16) -> PyResult<Bound<'py, PyAny>> {
        let process = self.process.clone();
        future_into_py(py, async move {
            process.resize(rows, cols).await.map_err(process_error)
        })
    }

    #[pyo3(signature = (timeout=None))]
    fn wait<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let process = self.process.clone();
        future_into_py(py, async move {
            process
                .wait(timeout.map(Duration::from_secs_f64))
                .await
                .map_err(process_error)
        })
    }

    fn terminate<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = self.process.clone();
        future_into_py(py, async move {
            process.terminate().await.map_err(process_error)
        })
    }

    fn kill<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = self.process.clone();
        future_into_py(
            py,
            async move { process.kill().await.map_err(process_error) },
        )
    }

    fn pid<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = self.process.clone();
        future_into_py(
            py,
            async move { process.pid().await.map_err(process_error) },
        )
    }
    /// Deliver an interrupt (Ctrl+C / SIGINT) to the PTY child.
    fn send_interrupt<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = self.process.clone();
        future_into_py(py, async move {
            process.send_interrupt().await.map_err(process_error)
        })
    }

    /// Gracefully terminate the PTY child and its descendants.
    fn terminate_tree<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = self.process.clone();
        future_into_py(py, async move {
            process.terminate_tree().await.map_err(process_error)
        })
    }

    /// Forcefully kill the PTY child and its descendants.
    fn kill_tree<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = self.process.clone();
        future_into_py(py, async move {
            process.kill_tree().await.map_err(process_error)
        })
    }

    /// Reply to any terminal capability queries present in a PTY output chunk.
    fn respond_to_queries<'py>(
        &self,
        py: Python<'py>,
        data: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let process = self.process.clone();
        future_into_py(py, async move {
            process
                .respond_to_queries(data)
                .await
                .map_err(process_error)
        })
    }

    /// Wait for exit, then drain output still in flight.
    #[pyo3(signature = (timeout=None, drain_timeout=1.0))]
    fn wait_and_drain<'py>(
        &self,
        py: Python<'py>,
        timeout: Option<f64>,
        drain_timeout: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let process = self.process.clone();
        future_into_py(py, async move {
            process
                .wait_and_drain(
                    timeout.map(Duration::from_secs_f64),
                    Duration::from_secs_f64(drain_timeout),
                )
                .await
                .map_err(process_error)
        })
    }

    /// Wait until the PTY reader closes. `False` means the wait timed out.
    #[pyo3(signature = (timeout=None))]
    fn wait_for_reader_closed<'py>(
        &self,
        py: Python<'py>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let process = self.process.clone();
        future_into_py(py, async move {
            process
                .wait_for_reader_closed(timeout.map(Duration::from_secs_f64))
                .await
                .map_err(process_error)
        })
    }

    /// Start relaying host terminal input into the PTY.
    fn start_terminal_input_relay<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = self.process.clone();
        future_into_py(py, async move {
            process
                .start_terminal_input_relay()
                .await
                .map_err(process_error)
        })
    }

    /// Stop the terminal input relay and wait for its worker to finish.
    fn stop_terminal_input_relay<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = self.process.clone();
        future_into_py(py, async move {
            process
                .stop_terminal_input_relay()
                .await
                .map_err(process_error)
        })
    }

    // The remainder are plain synchronous methods, not awaitables. They touch
    // only atomics, so wrapping them in a future would add scheduling cost and
    // an await to every call site for no gain -- and it would make them
    // unusable from the teardown paths (signal handlers, __del__) that need
    // them most, because those cannot await.

    /// Ask the terminal input relay to stop without waiting for it.
    fn request_terminal_input_relay_stop(&self) {
        self.process.request_terminal_input_relay_stop();
    }

    /// Whether the terminal input relay is currently running.
    fn terminal_input_relay_active(&self) -> bool {
        self.process.terminal_input_relay_active()
    }

    /// Set whether PTY input is echoed back to the host.
    fn set_echo(&self, enabled: bool) {
        self.process.set_echo(enabled);
    }

    /// Whether PTY input echo is enabled.
    fn echo_enabled(&self) -> bool {
        self.process.echo_enabled()
    }

    /// Close the PTY without waiting for bounded teardown to finish.
    fn close_nonblocking(&self) {
        self.process.close_nonblocking();
    }

    /// Record that the PTY reader has closed.
    fn mark_reader_closed(&self) {
        self.process.mark_reader_closed();
    }

    /// Record a child exit code observed out of band.
    fn store_returncode(&self, code: i32) {
        self.process.store_returncode(code);
    }

    /// Account for bytes written to the PTY by a path outside this handle.
    fn record_input_metrics(&self, data: Vec<u8>, submit: bool) {
        self.process.record_input_metrics(&data, submit);
    }

    /// Total bytes written to the PTY input stream.
    fn pty_input_bytes_total(&self) -> usize {
        self.process.pty_input_bytes_total()
    }

    /// Total newline events observed on PTY input.
    fn pty_newline_events_total(&self) -> usize {
        self.process.pty_newline_events_total()
    }

    /// Total submit events observed on PTY input.
    fn pty_submit_events_total(&self) -> usize {
        self.process.pty_submit_events_total()
    }

    /// Total bytes read from the PTY output stream.
    fn pty_output_bytes_total(&self) -> usize {
        self.process.pty_output_bytes_total()
    }

    /// Total terminal-control bytes seen in PTY output.
    fn pty_control_churn_bytes_total(&self) -> usize {
        self.process.pty_control_churn_bytes_total()
    }
}

fn output_tuple(output: RunOutput) -> (i32, Vec<u8>, Vec<u8>) {
    (output.exit_code, output.stdout, output.stderr)
}

fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or({
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            -status.signal().unwrap_or(1)
        }
        #[cfg(not(unix))]
        {
            -1
        }
    })
}

/// Python awaitable cursor over the output an actor has retained.
///
/// The async counterpart of the sync drain/read family. Those methods hand a
/// caller whatever has accumulated; a cursor instead gives each reader an
/// independent position in one shared bounded log, so two consumers cannot
/// steal records from each other and a slow one is told it fell behind rather
/// than silently skipping.
#[pyclass(module = "running_process._native")]
pub(crate) struct AsyncOutputCursor {
    cursor: Arc<Mutex<SharedOutputCursor>>,
}

#[pymethods]
impl AsyncOutputCursor {
    /// Await the next record, gap, or terminal EOF.
    ///
    /// Returns `("record", sequence, stream, data)`, `("gap", from, to, b"")`,
    /// or `None` at EOF. A gap is reported rather than skipped: the retention
    /// window advancing past a reader is data loss, and a cursor that hid it
    /// would let a consumer believe it had seen everything.
    fn read_next<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let cursor = Arc::clone(&self.cursor);
        future_into_py(py, async move {
            let read = cursor.lock().await.read_next_async().await;
            Ok(cursor_read_tuple(read))
        })
    }

    /// The next sequence this cursor will request. Synchronous: a counter read.
    fn position(&self) -> PyResult<u64> {
        Ok(self
            .cursor
            .try_lock()
            .map_err(|_| PyRuntimeError::new_err("cursor is busy in another read"))?
            .position())
    }

    /// Whether the producer has closed the log. Synchronous: an atomic load.
    fn is_closed(&self) -> PyResult<bool> {
        Ok(self
            .cursor
            .try_lock()
            .map_err(|_| PyRuntimeError::new_err("cursor is busy in another read"))?
            .is_closed())
    }
}

/// Flatten a `CursorRead` into one fixed tuple shape.
///
/// `("record", sequence, 0, stream, bytes)` or `("gap", from, to, "", b"")`,
/// with `None` for EOF. One shape rather than three keeps the PyO3 return type
/// concrete; the Python wrapper turns it straight back into typed objects, so
/// no caller ever sees this encoding.
fn cursor_read_tuple(read: CursorRead) -> Option<(String, u64, u64, String, Vec<u8>)> {
    match read {
        CursorRead::Record(record) => Some((
            "record".to_string(),
            record.sequence,
            0,
            match record.stream {
                StreamKind::Stdout => "stdout".to_string(),
                StreamKind::Stderr => "stderr".to_string(),
            },
            record.bytes,
        )),
        CursorRead::Gap { from, to } => {
            Some(("gap".to_string(), from, to, String::new(), Vec::new()))
        }
        CursorRead::Eof => None,
    }
}
