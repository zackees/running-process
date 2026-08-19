use pyo3::exceptions::{PyRuntimeError, PyTimeoutError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use running_process::pty::terminal_input::{
    TerminalInputCore, TerminalInputError, TerminalInputEventRecord,
};

use crate::helpers::to_py_err;

#[pyclass(skip_from_py_object)]
#[derive(Clone)]
pub(crate) struct NativeTerminalInputEvent {
    pub(crate) data: Vec<u8>,
    pub(crate) submit: bool,
    pub(crate) shift: bool,
    pub(crate) ctrl: bool,
    pub(crate) alt: bool,
    pub(crate) virtual_key_code: u16,
    pub(crate) repeat_count: u16,
}

#[pyclass]
pub(crate) struct NativeTerminalInput {
    pub(crate) inner: TerminalInputCore,
}

impl NativeTerminalInput {
    fn event_to_py(
        py: Python<'_>,
        event: TerminalInputEventRecord,
    ) -> PyResult<Py<NativeTerminalInputEvent>> {
        Py::new(
            py,
            NativeTerminalInputEvent {
                data: event.data,
                submit: event.submit,
                shift: event.shift,
                ctrl: event.ctrl,
                alt: event.alt,
                virtual_key_code: event.virtual_key_code,
                repeat_count: event.repeat_count,
            },
        )
    }

    fn wait_for_event(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> PyResult<TerminalInputEventRecord> {
        py.detach(|| {
            self.inner.wait_for_event(timeout).map_err(|err| match err {
                TerminalInputError::Closed => {
                    PyRuntimeError::new_err("Native terminal input is closed")
                }
                TerminalInputError::Timeout => {
                    PyTimeoutError::new_err("No terminal input available before timeout")
                }
                other => to_py_err(other),
            })
        })
    }
}

#[pymethods]
impl NativeTerminalInputEvent {
    #[getter]
    fn data(&self, py: Python<'_>) -> Py<PyAny> {
        PyBytes::new(py, &self.data).into_any().unbind()
    }

    #[getter]
    fn submit(&self) -> bool {
        self.submit
    }

    #[getter]
    fn shift(&self) -> bool {
        self.shift
    }

    #[getter]
    fn ctrl(&self) -> bool {
        self.ctrl
    }

    #[getter]
    fn alt(&self) -> bool {
        self.alt
    }

    #[getter]
    fn virtual_key_code(&self) -> u16 {
        self.virtual_key_code
    }

    #[getter]
    fn repeat_count(&self) -> u16 {
        self.repeat_count
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "NativeTerminalInputEvent(data={:?}, submit={}, shift={}, ctrl={}, alt={}, virtual_key_code={}, repeat_count={})",
            self.data,
            self.submit,
            self.shift,
            self.ctrl,
            self.alt,
            self.virtual_key_code,
            self.repeat_count,
        )
    }
}

#[pymethods]
impl NativeTerminalInput {
    #[new]
    pub(crate) fn new() -> Self {
        Self {
            inner: TerminalInputCore::new(),
        }
    }

    pub(crate) fn start(&self) -> PyResult<()> {
        #[cfg(windows)]
        {
            self.inner.start_impl().map_err(to_py_err)
        }

        #[cfg(not(windows))]
        {
            Err(PyRuntimeError::new_err(
                "NativeTerminalInput is only available on Windows consoles",
            ))
        }
    }

    fn stop(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.inner.stop_impl().map_err(to_py_err))
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.inner.stop_impl().map_err(to_py_err))
    }

    pub(crate) fn available(&self) -> bool {
        self.inner.available()
    }

    #[getter]
    pub(crate) fn capturing(&self) -> bool {
        self.inner.capturing()
    }

    #[getter]
    fn original_console_mode(&self) -> Option<u32> {
        self.inner.original_console_mode()
    }

    #[getter]
    fn active_console_mode(&self) -> Option<u32> {
        self.inner.active_console_mode()
    }

    #[pyo3(signature = (timeout=None))]
    fn read_event(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> PyResult<Py<NativeTerminalInputEvent>> {
        let event = self.wait_for_event(py, timeout)?;
        Self::event_to_py(py, event)
    }

    fn read_event_non_blocking(
        &self,
        py: Python<'_>,
    ) -> PyResult<Option<Py<NativeTerminalInputEvent>>> {
        if let Some(event) = self.inner.next_event() {
            return Self::event_to_py(py, event).map(Some);
        }
        if self
            .inner
            .state
            .lock()
            .expect("terminal input mutex poisoned")
            .closed
        {
            return Err(PyRuntimeError::new_err("Native terminal input is closed"));
        }
        Ok(None)
    }

    #[pyo3(signature = (timeout=None))]
    fn read(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<Py<PyAny>> {
        let event = self.wait_for_event(py, timeout)?;
        Ok(PyBytes::new(py, &event.data).into_any().unbind())
    }

    fn read_non_blocking(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        if let Some(event) = self.inner.next_event() {
            return Ok(Some(PyBytes::new(py, &event.data).into_any().unbind()));
        }
        if self
            .inner
            .state
            .lock()
            .expect("terminal input mutex poisoned")
            .closed
        {
            return Err(PyRuntimeError::new_err("Native terminal input is closed"));
        }
        Ok(None)
    }

    fn drain(&self, py: Python<'_>) -> Vec<Py<PyAny>> {
        self.inner
            .drain_events()
            .into_iter()
            .map(|event| PyBytes::new(py, &event.data).into_any().unbind())
            .collect()
    }

    fn drain_events(&self, py: Python<'_>) -> PyResult<Vec<Py<NativeTerminalInputEvent>>> {
        self.inner
            .drain_events()
            .into_iter()
            .map(|event| Self::event_to_py(py, event))
            .collect()
    }

    /// Wait for at least one input event, then drain all queued events and
    /// return their data merged into a single `bytes` object plus a `submit`
    /// flag.  This avoids per-event Python round-trips during large pastes.
    ///
    /// Returns ``(data: bytes, submit: bool)``.
    #[pyo3(signature = (timeout=None))]
    fn read_batch(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<(Py<PyAny>, bool)> {
        // Block (releasing the GIL) until the first event arrives.
        let first = self.wait_for_event(py, timeout)?;

        // Drain everything else already queued.
        let remaining = self.inner.drain_events();

        // Merge all data into one buffer.
        let capacity = first.data.len() + remaining.iter().map(|e| e.data.len()).sum::<usize>();
        let mut merged = Vec::with_capacity(capacity);
        let mut submit = first.submit;
        merged.extend_from_slice(&first.data);
        for event in &remaining {
            merged.extend_from_slice(&event.data);
            submit = submit || event.submit;
        }

        Ok((PyBytes::new(py, &merged).into_any().unbind(), submit))
    }
}

// Drop is now handled by TerminalInputCore's Drop impl

#[cfg(test)]
mod wrapper_tests {
    use super::*;

    fn event(data: &[u8], submit: bool) -> TerminalInputEventRecord {
        TerminalInputEventRecord {
            data: data.to_vec(),
            submit,
            shift: true,
            ctrl: false,
            alt: true,
            virtual_key_code: 13,
            repeat_count: 2,
        }
    }

    fn enqueue(input: &NativeTerminalInput, values: &[(&[u8], bool)]) {
        let mut state = input.inner.state.lock().unwrap();
        state.closed = false;
        state
            .events
            .extend(values.iter().map(|(data, submit)| event(data, *submit)));
        input.inner.condvar.notify_all();
    }

    #[test]
    fn event_getters_and_python_data_cover_the_complete_payload() {
        pyo3::Python::initialize();
        pyo3::Python::attach(|py| {
            let event = NativeTerminalInputEvent {
                data: b"enter".to_vec(),
                submit: true,
                shift: true,
                ctrl: false,
                alt: true,
                virtual_key_code: 13,
                repeat_count: 2,
            };
            let _ = event.data(py);
            assert!(event.submit());
            assert!(event.shift());
            assert!(!event.ctrl());
            assert!(event.alt());
            assert_eq!(event.virtual_key_code(), 13);
            assert_eq!(event.repeat_count(), 2);
            assert!(event.__repr__().contains("submit=true"));
        });
    }

    #[test]
    fn queue_read_drain_and_batch_wrappers_cover_each_python_shape() {
        pyo3::Python::initialize();
        pyo3::Python::attach(|py| {
            let input = NativeTerminalInput::new();
            assert!(!input.available());
            assert!(!input.capturing());
            assert_eq!(input.original_console_mode(), None);
            assert_eq!(input.active_console_mode(), None);

            enqueue(&input, &[(b"event", false)]);
            assert!(input.read_event_non_blocking(py).unwrap().is_some());
            enqueue(&input, &[(b"bytes", false)]);
            assert!(input.read_non_blocking(py).unwrap().is_some());

            enqueue(&input, &[(b"a", false), (b"b", true)]);
            assert_eq!(input.drain(py).len(), 2);
            enqueue(&input, &[(b"a", false), (b"b", true)]);
            assert_eq!(input.drain_events(py).unwrap().len(), 2);

            enqueue(&input, &[(b"blocking-event", false)]);
            assert!(input.read_event(py, Some(0.1)).is_ok());
            enqueue(&input, &[(b"blocking-bytes", false)]);
            assert!(input.read(py, Some(0.1)).is_ok());
            enqueue(&input, &[(b"first", false), (b"second", true)]);
            let (_, submit) = input.read_batch(py, Some(0.1)).unwrap();
            assert!(submit);

            input.stop(py).unwrap();
            input.close(py).unwrap();
            assert!(input.read_non_blocking(py).is_err());
            assert!(input.read_event_non_blocking(py).is_err());
        });
    }

    #[test]
    fn empty_open_queue_reports_timeout() {
        pyo3::Python::initialize();
        pyo3::Python::attach(|py| {
            let input = NativeTerminalInput::new();
            input.inner.state.lock().unwrap().closed = false;
            let error = input.read_event(py, Some(0.0)).unwrap_err();
            assert!(error.is_instance_of::<PyTimeoutError>(py));
        });
    }
}
