//! Native Python awaitables backed by the Rust async process and PTY APIs.

use std::ffi::OsString;
use std::time::Duration;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use pyo3_async_runtimes::tokio::future_into_py;
use running_process::pty::AsyncPtyProcess;
use running_process::{AsyncProcess, RunOutput};

fn process_error(error: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

/// Python awaitable process facade backed by native Rust futures.
#[pyclass(module = "running_process._native")]
pub(crate) struct AsyncRunningProcess {
    program: OsString,
    args: Vec<OsString>,
}

#[pymethods]
impl AsyncRunningProcess {
    #[new]
    fn new(program: String, args: Vec<String>) -> Self {
        Self {
            program: OsString::from(program),
            args: args.into_iter().map(OsString::from).collect(),
        }
    }

    /// Return an awaitable yielding `(exit_code, stdout_bytes, stderr_bytes)`.
    fn run<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let program = self.program.clone();
        let args = self.args.clone();
        future_into_py(py, async move {
            let output = AsyncProcess::run(program, &args)
                .await
                .map_err(process_error)?;
            Ok(output_tuple(output))
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
}

fn output_tuple(output: RunOutput) -> (i32, Vec<u8>, Vec<u8>) {
    (output.exit_code, output.stdout, output.stderr)
}
