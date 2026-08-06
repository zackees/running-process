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
use running_process::{AsyncProcess, RunOutput};
use tokio::sync::Mutex;

fn process_error(error: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

/// Python awaitable process facade backed by native Rust futures.
#[pyclass(module = "running_process._native")]
pub(crate) struct AsyncRunningProcess {
    process: Arc<Mutex<AsyncProcess>>,
}

#[pymethods]
impl AsyncRunningProcess {
    #[new]
    fn new(program: String, args: Vec<String>) -> Self {
        let mut process = AsyncProcess::new(OsString::from(program));
        for arg in args {
            process = process.arg(OsString::from(arg));
        }
        Self {
            process: Arc::new(Mutex::new(process)),
        }
    }

    /// Start the configured child and return when the actor owns it.
    fn start<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            process.lock().await.start().await.map_err(process_error)
        })
    }

    /// Return the child pid.
    fn pid<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            process.lock().await.pid().await.map_err(process_error)
        })
    }

    /// Wait for exit and return the platform-normalized exit code.
    fn wait<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            process
                .lock()
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
                .lock()
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
            process.lock().await.kill().await.map_err(process_error)
        })
    }

    /// Write bytes to stdin.
    fn write_stdin<'py>(&self, py: Python<'py>, bytes: Vec<u8>) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            process
                .lock()
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
                .lock()
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
                .lock()
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
                .lock()
                .await
                .output_bounded(limit)
                .await
                .map(output_tuple)
                .map_err(process_error)
        })
    }

    /// Spawn, wait, and return `(exit_code, stdout_bytes, stderr_bytes)`.
    fn run<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let process = Arc::clone(&self.process);
        future_into_py(py, async move {
            let mut process = process.lock().await;
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
