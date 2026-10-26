//! Python bindings for the supervised service SDK
//! (`running_process::broker::backend_sdk::{ServiceServer, PollingFrameClient}`).
//!
//! The Python facade is the top-level `running_process_service` package;
//! these classes are its private native half. Every client method is
//! non-blocking and runs without releasing the GIL, because each one only
//! touches an in-memory queue; `NativeServiceServer.recv` waits, so it
//! detaches from the interpreter while it does.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pyo3::exceptions::{PyFileExistsError, PyOSError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use running_process::broker::backend_sdk::{
    PollingFrameClient, SendError, ServiceError, ServiceRequest, ServiceServer, ServiceSpec,
    ServiceState,
};

/// `send` result codes for a refused request (non-negative values are ids).
const SEND_UNAVAILABLE: i64 = -1;
const SEND_QUEUE_FULL: i64 = -2;

fn seconds(name: &str, value: f64) -> PyResult<Duration> {
    if !value.is_finite() || value < 0.0 {
        return Err(PyValueError::new_err(format!(
            "{name} must be a non-negative number of seconds"
        )));
    }
    Ok(Duration::from_secs_f64(value))
}

fn service_error(error: ServiceError) -> PyErr {
    match error {
        ServiceError::InvalidName(_) | ServiceError::InvalidPayloadProtocol(_) => {
            PyValueError::new_err(error.to_string())
        }
        ServiceError::AlreadyRunning { .. } => PyFileExistsError::new_err(error.to_string()),
        ServiceError::Io { .. } => PyOSError::new_err(error.to_string()),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_spec(
    name: String,
    version: String,
    payload_protocol: u32,
    command: Option<Vec<String>>,
    env: Option<Vec<(String, String)>>,
    cwd: Option<String>,
    runtime_dir: Option<String>,
    health_interval: f64,
    spawn_attempts: u32,
    spawn_window: f64,
    spawn_ready_timeout: f64,
) -> PyResult<ServiceSpec> {
    Ok(ServiceSpec::new(name, version, payload_protocol)
        .map_err(service_error)?
        .command(command)
        .env(env.unwrap_or_default())
        .cwd(cwd.map(PathBuf::from))
        .runtime_dir(runtime_dir.map(PathBuf::from))
        .health_interval(seconds("health_interval", health_interval)?)
        .spawn_budget(
            spawn_attempts,
            seconds("spawn_budget window", spawn_window)?,
        )
        .spawn_ready_timeout(seconds("spawn_ready_timeout", spawn_ready_timeout)?))
}

/// Validate a spec and return its derived endpoint and identity path.
#[pyfunction]
#[pyo3(signature = (name, version, payload_protocol, runtime_dir=None))]
pub(crate) fn native_service_paths(
    name: String,
    version: String,
    payload_protocol: u32,
    runtime_dir: Option<String>,
) -> PyResult<(String, String)> {
    let spec = ServiceSpec::new(name, version, payload_protocol)
        .map_err(service_error)?
        .runtime_dir(runtime_dir.map(PathBuf::from));
    let endpoint = spec.endpoint().map_err(service_error)?;
    Ok((
        endpoint.path,
        spec.identity_path().to_string_lossy().into_owned(),
    ))
}

/// One request received by a `NativeServiceServer`.
#[pyclass(module = "running_process._native", frozen)]
pub(crate) struct NativeServiceRequest {
    request_id: u64,
    payload: Vec<u8>,
    inner: Mutex<Option<ServiceRequest>>,
}

#[pymethods]
impl NativeServiceRequest {
    #[getter]
    fn request_id(&self) -> u64 {
        self.request_id
    }

    #[getter]
    fn payload<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.payload)
    }

    /// Send the reply. Returns False if already replied or the client left.
    fn reply(&self, py: Python<'_>, payload: Vec<u8>) -> bool {
        let Some(request) = self.inner.lock().ok().and_then(|mut slot| slot.take()) else {
            return false;
        };
        py.detach(|| request.reply(payload).is_ok())
    }
}

/// A bound single-instance service endpoint.
#[pyclass(module = "running_process._native", frozen)]
pub(crate) struct NativeServiceServer {
    inner: Arc<ServiceServer>,
}

#[pymethods]
impl NativeServiceServer {
    #[new]
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (name, version, payload_protocol, runtime_dir=None))]
    fn new(
        py: Python<'_>,
        name: String,
        version: String,
        payload_protocol: u32,
        runtime_dir: Option<String>,
    ) -> PyResult<Self> {
        let spec = build_spec(
            name,
            version,
            payload_protocol,
            None,
            None,
            None,
            runtime_dir,
            5.0,
            0,
            0.0,
            0.0,
        )?;
        let server = py
            .detach(|| ServiceServer::bind(spec))
            .map_err(service_error)?;
        Ok(Self {
            inner: Arc::new(server),
        })
    }

    /// Wait up to `timeout` seconds (forever when None) for a request.
    #[pyo3(signature = (timeout=None))]
    fn recv(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<Option<NativeServiceRequest>> {
        let timeout = timeout.map(|t| seconds("timeout", t)).transpose()?;
        let server = Arc::clone(&self.inner);
        let request = py.detach(move || server.recv(timeout));
        Ok(request.map(|request| NativeServiceRequest {
            request_id: request.request_id(),
            payload: request.payload().to_vec(),
            inner: Mutex::new(Some(request)),
        }))
    }

    /// Stop accepting, remove the identity file, release the endpoint.
    fn close(&self, py: Python<'_>) {
        let server = Arc::clone(&self.inner);
        py.detach(move || server.close());
    }
}

/// The non-blocking client. Every method returns immediately.
#[pyclass(module = "running_process._native", frozen)]
pub(crate) struct NativeServiceClient {
    inner: PollingFrameClient,
}

#[pymethods]
impl NativeServiceClient {
    #[new]
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (
        name, version, payload_protocol, *, command=None, env=None, cwd=None,
        runtime_dir=None, health_interval=5.0, spawn_attempts=3, spawn_window=30.0,
        spawn_ready_timeout=10.0, queue_depth=64
    ))]
    fn new(
        name: String,
        version: String,
        payload_protocol: u32,
        command: Option<Vec<String>>,
        env: Option<Vec<(String, String)>>,
        cwd: Option<String>,
        runtime_dir: Option<String>,
        health_interval: f64,
        spawn_attempts: u32,
        spawn_window: f64,
        spawn_ready_timeout: f64,
        queue_depth: usize,
    ) -> PyResult<Self> {
        let spec = build_spec(
            name,
            version,
            payload_protocol,
            command,
            env,
            cwd,
            runtime_dir,
            health_interval,
            spawn_attempts,
            spawn_window,
            spawn_ready_timeout,
        )?;
        let inner = PollingFrameClient::connect(spec, queue_depth)
            .map_err(|error| PyOSError::new_err(error.to_string()))?;
        Ok(Self { inner })
    }

    /// "starting", "up" or "down".
    #[getter]
    fn state(&self) -> &'static str {
        match self.inner.state() {
            ServiceState::Starting => "starting",
            ServiceState::Up => "up",
            ServiceState::Down => "down",
        }
    }

    /// Queue a request: its id, or -1 (unavailable) / -2 (queue full).
    fn send(&self, payload: Vec<u8>) -> i64 {
        match self.inner.send(payload) {
            Ok(id) => i64::try_from(id).unwrap_or(i64::MAX),
            Err(SendError::Unavailable) => SEND_UNAVAILABLE,
            Err(SendError::QueueFull) => SEND_QUEUE_FULL,
        }
    }

    /// The oldest reply as (request_id, payload, error), or None.
    fn try_recv<'py>(
        &self,
        py: Python<'py>,
    ) -> Option<(u64, Option<Bound<'py, PyBytes>>, Option<String>)> {
        let reply = self.inner.try_recv()?;
        Some(match reply.result {
            Ok(payload) => (reply.request_id, Some(PyBytes::new(py, &payload)), None),
            Err(error) => (reply.request_id, None, Some(error)),
        })
    }

    fn close(&self) {
        self.inner.close();
    }
}
