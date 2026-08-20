use std::path::PathBuf;
use std::time::{Duration, Instant};
use std::{collections::HashMap, sync::atomic::AtomicU64};

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyString};
use regex::Regex;

#[cfg(unix)]
use running_process::{unix_signal_process, unix_signal_process_group, UnixSignal};
use running_process::{
    NativeProcess, ObservationPolicy, ProcessConfig, ProcessEventKind, ProcessWatch,
    ProcessWatchCursor, ProcessWatchMatch, ProcessWatchRead, ProcessWatchSubscriber, ReadStatus,
    StackCapture, StackDump, StreamEvent, StreamKind,
};

use crate::helpers::{
    parse_command, process_err_to_py, stderr_mode, stdin_mode, stream_kind, to_py_err,
};
use crate::public_symbols;
use crate::registry::{ExpectDetails, ExpectResult};

fn parse_observation_policy(value: &str) -> PyResult<ObservationPolicy> {
    match value {
        "non_invasive" => Ok(ObservationPolicy::NonInvasive),
        "allow_tracing" => Ok(ObservationPolicy::AllowTracing),
        "require_exact" => Ok(ObservationPolicy::RequireExact),
        _ => Err(PyValueError::new_err(format!(
            "unknown process observation policy: {value}"
        ))),
    }
}

fn optional_string(mapping: &Bound<'_, PyDict>, key: &str) -> PyResult<Option<String>> {
    let Some(value) = mapping.get_item(key)? else {
        return Ok(None);
    };
    if value.is_none() {
        Ok(None)
    } else {
        value.extract().map(Some)
    }
}

fn optional_i32(mapping: &Bound<'_, PyDict>, key: &str) -> PyResult<Option<i32>> {
    let Some(value) = mapping.get_item(key)? else {
        return Ok(None);
    };
    if value.is_none() {
        Ok(None)
    } else {
        value.extract().map(Some)
    }
}

fn optional_limit(mapping: &Bound<'_, PyDict>) -> PyResult<Option<usize>> {
    let Some(value) = mapping.get_item("limit")? else {
        return Ok(Some(1));
    };
    if value.is_none() {
        Ok(None)
    } else {
        value.extract().map(Some)
    }
}

fn parse_stack_dump(mapping: &Bound<'_, PyDict>) -> PyResult<Option<StackDump>> {
    let Some(capture) = optional_string(mapping, "dump_capture")? else {
        return Ok(None);
    };
    let capture = match capture.as_str() {
        "origin_preferred" => StackCapture::OriginPreferred,
        "origin_required" => StackCapture::OriginRequired,
        "owner_all_threads" => StackCapture::OwnerAllThreads,
        _ => {
            return Err(PyValueError::new_err(format!(
                "unknown stack capture policy: {capture}"
            )))
        }
    };
    let symbolize =
        optional_string(mapping, "dump_symbolize")?.unwrap_or_else(|| "deferred".to_owned());
    if !matches!(symbolize.as_str(), "deferred" | "immediate") {
        return Err(PyValueError::new_err(
            "dump symbolize must be 'deferred' or 'immediate'",
        ));
    }
    Ok(Some(StackDump {
        capture,
        directory: optional_string(mapping, "dump_directory")?.map(PathBuf::from),
        symbolize_immediately: symbolize == "immediate",
    }))
}

fn parse_process_watch(mapping: &Bound<'_, PyDict>) -> PyResult<ProcessWatch> {
    let kind = mapping
        .get_item("kind")?
        .ok_or_else(|| PyValueError::new_err("process watch is missing kind"))?
        .extract::<String>()?;
    let label = mapping
        .get_item("label")?
        .ok_or_else(|| PyValueError::new_err("process watch is missing label"))?
        .extract::<String>()?;
    let cooldown_seconds = mapping
        .get_item("cooldown_seconds")?
        .map(|value| value.extract::<f64>())
        .transpose()?
        .unwrap_or(0.0);
    if !cooldown_seconds.is_finite() || cooldown_seconds < 0.0 {
        return Err(PyValueError::new_err(
            "cooldown_seconds must be a finite non-negative number",
        ));
    }
    let cooldown = Duration::from_secs_f64(cooldown_seconds);
    let limit = optional_limit(mapping)?;
    let dump = parse_stack_dump(mapping)?;
    let result = match kind.as_str() {
        "spawn" => ProcessWatch::on_spawn(dump, limit, cooldown, label),
        "exec" => ProcessWatch::on_exec(
            optional_string(mapping, "basename")?,
            optional_string(mapping, "path")?.map(PathBuf::from),
            dump,
            limit,
            cooldown,
            label,
        ),
        "exit" => ProcessWatch::on_exit(
            optional_i32(mapping, "code")?,
            optional_i32(mapping, "signal")?,
            optional_string(mapping, "basename")?,
            dump,
            limit,
            cooldown,
            label,
        ),
        "failure" => ProcessWatch::on_failure(
            optional_string(mapping, "basename")?,
            dump,
            limit,
            cooldown,
            label,
        ),
        _ => {
            return Err(PyValueError::new_err(format!(
                "unknown process watch kind: {kind}"
            )))
        }
    };
    result.map_err(|error| PyValueError::new_err(error.to_string()))
}

fn event_kind_name(kind: ProcessEventKind) -> &'static str {
    match kind {
        ProcessEventKind::Spawn => "spawn",
        ProcessEventKind::Exec => "exec",
        ProcessEventKind::Exit => "exit",
        ProcessEventKind::Loss => "loss",
    }
}

pub(crate) fn watch_match_to_python(
    py: Python<'_>,
    item: &ProcessWatchMatch,
) -> PyResult<Py<PyAny>> {
    let result = PyDict::new(py);
    result.set_item("type", "match")?;
    result.set_item("sequence", item.sequence)?;
    result.set_item("watch_label", &item.watch.label)?;
    result.set_item("event", watch_event_to_python(py, &item.event)?)?;
    if let Some(dump) = item.dump.as_ref() {
        let value = PyDict::new(py);
        value.set_item("capture_source", dump.capture_source.as_str())?;
        value.set_item(
            "artifacts",
            dump.artifacts
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
        )?;
        value.set_item("symbolized", dump.symbolized)?;
        value.set_item("error", dump.error.as_deref())?;
        result.set_item("dump", value)?;
    } else {
        result.set_item("dump", py.None())?;
    }
    Ok(result.into_any().unbind())
}

fn watch_event_to_python(
    py: Python<'_>,
    item: &running_process::ProcessEvent,
) -> PyResult<Py<PyAny>> {
    let event = PyDict::new(py);
    event.set_item("kind", event_kind_name(item.kind))?;
    event.set_item("pid", item.process.pid)?;
    event.set_item("start_key", item.process.start_key)?;
    event.set_item("parent_pid", item.parent.as_ref().map(|parent| parent.pid))?;
    event.set_item(
        "parent_start_key",
        item.parent.as_ref().and_then(|parent| parent.start_key),
    )?;
    let timestamp = item
        .timestamp
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    event.set_item("timestamp", timestamp)?;
    event.set_item(
        "executable",
        item.executable
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
    )?;
    event.set_item("argv", item.argv.as_ref())?;
    event.set_item("exit_code", item.exit_code)?;
    event.set_item("signal", item.signal)?;
    event.set_item("raw_exit_status", item.raw_exit_status)?;
    event.set_item("backend", item.backend)?;
    event.set_item("observation_grade", item.observation_grade.as_str())?;
    event.set_item("coverage_complete", item.coverage_complete)?;
    event.set_item("loss_detected", item.loss_detected)?;
    Ok(event.into_any().unbind())
}

pub(crate) fn watch_read_to_python(py: Python<'_>, read: ProcessWatchRead) -> PyResult<Py<PyAny>> {
    match read {
        ProcessWatchRead::Match(item) => watch_match_to_python(py, &item),
        ProcessWatchRead::Loss(item) => {
            let result = PyDict::new(py);
            result.set_item("type", "loss")?;
            result.set_item("sequence", item.sequence)?;
            result.set_item("reason", &item.reason)?;
            result.set_item("event", watch_event_to_python(py, &item.event)?)?;
            Ok(result.into_any().unbind())
        }
        ProcessWatchRead::Gap(gap) => {
            let result = PyDict::new(py);
            result.set_item("type", "gap")?;
            result.set_item("first_missing", gap.first_missing)?;
            result.set_item("last_missing", gap.last_missing)?;
            Ok(result.into_any().unbind())
        }
        ProcessWatchRead::Timeout => {
            let result = PyDict::new(py);
            result.set_item("type", "timeout")?;
            Ok(result.into_any().unbind())
        }
        ProcessWatchRead::Eof => {
            let result = PyDict::new(py);
            result.set_item("type", "eof")?;
            Ok(result.into_any().unbind())
        }
    }
}

#[pyclass]
pub(crate) struct NativeRunningProcess {
    pub(crate) inner: NativeProcess,
    process_watch_subscriber: Option<ProcessWatchSubscriber>,
    process_watch_cursors:
        std::sync::Mutex<HashMap<u64, std::sync::Arc<std::sync::Mutex<ProcessWatchCursor>>>>,
    next_process_watch_cursor: AtomicU64,
    pub(crate) text: bool,
    pub(crate) encoding: Option<String>,
    pub(crate) errors: Option<String>,
    #[cfg(windows)]
    pub(crate) creationflags: Option<u32>,
    #[cfg(unix)]
    pub(crate) create_process_group: bool,
    pub(crate) owns_process_group: bool,
}

impl NativeRunningProcess {
    pub(crate) fn read_process_watch_native(
        &self,
        cursor_id: u64,
        timeout: Option<Duration>,
    ) -> Result<ProcessWatchRead, String> {
        let cursor = self
            .process_watch_cursors
            .lock()
            .expect("process watch cursors mutex poisoned")
            .get(&cursor_id)
            .cloned()
            .ok_or_else(|| "unknown process watch cursor".to_owned())?;
        let read = cursor
            .lock()
            .expect("process watch cursor mutex poisoned")
            .read_next(timeout);
        Ok(read)
    }
}

#[cfg(test)]
#[path = "tests/process_wrapper.rs"]
mod wrapper_tests;

#[pymethods]
impl NativeRunningProcess {
    #[new]
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (command, cwd=None, shell=false, capture=true, env=None, creationflags=None, text=true, encoding=None, errors=None, stdin_mode_name="inherit", stderr_mode_name="stdout", nice=None, create_process_group=false, address_space_limit_bytes=None, process_watches=None, process_observation="non_invasive"))]
    pub(crate) fn new(
        command: &Bound<'_, PyAny>,
        cwd: Option<String>,
        shell: bool,
        capture: bool,
        env: Option<Bound<'_, PyDict>>,
        creationflags: Option<u32>,
        text: bool,
        encoding: Option<String>,
        errors: Option<String>,
        stdin_mode_name: &str,
        stderr_mode_name: &str,
        nice: Option<i32>,
        create_process_group: bool,
        address_space_limit_bytes: Option<u64>,
        process_watches: Option<Vec<Bound<'_, PyDict>>>,
        process_observation: &str,
    ) -> PyResult<Self> {
        let parsed = parse_command(command, shell)?;
        let env_pairs = env
            .map(|mapping| {
                mapping
                    .iter()
                    .map(|(key, value)| Ok((key.extract::<String>()?, value.extract::<String>()?)))
                    .collect::<PyResult<Vec<(String, String)>>>()
            })
            .transpose()?;

        let config = ProcessConfig {
            command: parsed,
            cwd: cwd.map(PathBuf::from),
            env: env_pairs,
            capture,
            stderr_mode: stderr_mode(stderr_mode_name)?,
            creationflags,
            create_process_group,
            stdin_mode: stdin_mode(stdin_mode_name)?,
            nice,
            address_space_limit_bytes,
        };
        let watches = process_watches
            .unwrap_or_default()
            .iter()
            .map(parse_process_watch)
            .collect::<PyResult<Vec<_>>>()?;
        let policy = parse_observation_policy(process_observation)?;
        let (inner, process_watch_subscriber) = if watches.is_empty() {
            (NativeProcess::new(config), None)
        } else {
            let (process, subscriber) =
                NativeProcess::with_process_watches(config, watches, policy)
                    .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
            (process, Some(subscriber))
        };

        Ok(Self {
            inner,
            process_watch_subscriber,
            process_watch_cursors: std::sync::Mutex::new(HashMap::new()),
            next_process_watch_cursor: AtomicU64::new(1),
            text,
            encoding,
            errors,
            #[cfg(windows)]
            creationflags,
            #[cfg(unix)]
            create_process_group,
            owns_process_group: create_process_group,
        })
    }

    #[staticmethod]
    pub(crate) fn process_observation_capabilities(py: Python<'_>) -> PyResult<Py<PyAny>> {
        let capability = NativeProcess::process_observation_capabilities();
        let result = PyDict::new(py);
        result.set_item("exact_available", capability.exact_available)?;
        result.set_item("exact_backend", capability.exact_backend)?;
        result.set_item("reason", capability.reason)?;
        result.set_item("non_invasive_backend", capability.non_invasive_backend)?;
        result.set_item("non_invasive_grade", capability.non_invasive_grade.as_str())?;
        Ok(result.into_any().unbind())
    }

    pub(crate) fn process_observation(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        self.process_watch_subscriber
            .as_ref()
            .map(|subscriber| {
                let observation = subscriber.observation();
                let result = PyDict::new(py);
                result.set_item("backend", observation.backend)?;
                result.set_item("observation_grade", observation.grade.as_str())?;
                result.set_item("fallback_reason", observation.fallback_reason.as_deref())?;
                Ok(result.into_any().unbind())
            })
            .transpose()
    }

    pub(crate) fn process_watch_snapshot(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        self.process_watch_subscriber
            .as_ref()
            .map_or_else(Vec::new, ProcessWatchSubscriber::snapshot)
            .iter()
            .map(|item| watch_match_to_python(py, item))
            .collect()
    }

    pub(crate) fn open_process_watch_cursor(&self) -> Option<u64> {
        let subscriber = self.process_watch_subscriber.as_ref()?;
        let id = self
            .next_process_watch_cursor
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.process_watch_cursors
            .lock()
            .expect("process watch cursors mutex poisoned")
            .insert(
                id,
                std::sync::Arc::new(std::sync::Mutex::new(subscriber.cursor())),
            );
        Some(id)
    }

    #[pyo3(signature = (cursor_id, timeout=None))]
    pub(crate) fn take_process_watch_match(
        &self,
        py: Python<'_>,
        cursor_id: u64,
        timeout: Option<f64>,
    ) -> PyResult<Py<PyAny>> {
        if timeout.is_some_and(|value| !value.is_finite() || value < 0.0) {
            return Err(PyValueError::new_err(
                "timeout must be a finite non-negative number or None",
            ));
        }
        let timeout = timeout.map(Duration::from_secs_f64);
        let read = py.detach(|| self.read_process_watch_native(cursor_id, timeout));
        watch_read_to_python(py, read.map_err(PyValueError::new_err)?)
    }

    #[inline(never)]
    pub(crate) fn start(&self) -> PyResult<()> {
        public_symbols::rp_native_running_process_start_public(self)
    }

    pub(crate) fn poll(&self) -> PyResult<Option<i32>> {
        self.inner.poll().map_err(to_py_err)
    }

    #[pyo3(signature = (timeout=None))]
    #[inline(never)]
    pub(crate) fn wait(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<i32> {
        public_symbols::rp_native_running_process_wait_public(self, py, timeout)
    }

    #[inline(never)]
    pub(crate) fn kill(&self) -> PyResult<()> {
        public_symbols::rp_native_running_process_kill_public(self)
    }

    #[inline(never)]
    pub(crate) fn terminate(&self) -> PyResult<()> {
        public_symbols::rp_native_running_process_terminate_public(self)
    }

    #[inline(never)]
    pub(crate) fn close(&self, py: Python<'_>) -> PyResult<()> {
        public_symbols::rp_native_running_process_close_public(self, py)
    }

    pub(crate) fn terminate_group(&self) -> PyResult<()> {
        #[cfg(unix)]
        {
            let pid = self
                .inner
                .pid()
                .ok_or_else(|| PyRuntimeError::new_err("process is not running"))?;
            if self.create_process_group {
                unix_signal_process_group(pid as i32, UnixSignal::Terminate).map_err(to_py_err)?;
                return Ok(());
            }
        }
        self.inner.terminate().map_err(to_py_err)
    }

    pub(crate) fn write_stdin(&self, data: &[u8]) -> PyResult<()> {
        self.inner.write_stdin(data).map_err(to_py_err)
    }

    #[getter]
    pub(crate) fn pid(&self) -> Option<u32> {
        self.inner.pid()
    }

    #[getter]
    pub(crate) fn returncode(&self) -> Option<i32> {
        self.inner.returncode()
    }

    #[inline(never)]
    pub(crate) fn send_interrupt(&self) -> PyResult<()> {
        public_symbols::rp_native_running_process_send_interrupt_public(self)
    }

    pub(crate) fn kill_group(&self) -> PyResult<()> {
        #[cfg(unix)]
        {
            let pid = self
                .inner
                .pid()
                .ok_or_else(|| PyRuntimeError::new_err("process is not running"))?;
            if self.create_process_group {
                unix_signal_process_group(pid as i32, UnixSignal::Kill).map_err(to_py_err)?;
                return Ok(());
            }
        }
        self.inner.kill().map_err(to_py_err)
    }

    pub(crate) fn has_pending_combined(&self) -> bool {
        self.inner.has_pending_combined()
    }

    pub(crate) fn has_pending_stream(&self, stream: &str) -> PyResult<bool> {
        Ok(self.inner.has_pending_stream(stream_kind(stream)?))
    }

    pub(crate) fn drain_combined(&self, py: Python<'_>) -> PyResult<Vec<(String, Py<PyAny>)>> {
        self.inner
            .drain_combined()
            .into_iter()
            .map(|event| {
                Ok((
                    event.stream.as_str().to_string(),
                    self.decode_line(py, &event.line)?,
                ))
            })
            .collect()
    }

    pub(crate) fn drain_stream(&self, py: Python<'_>, stream: &str) -> PyResult<Vec<Py<PyAny>>> {
        self.inner
            .drain_stream(stream_kind(stream)?)
            .into_iter()
            .map(|line| self.decode_line(py, &line))
            .collect()
    }

    #[pyo3(signature = (timeout=None))]
    pub(crate) fn take_combined_line(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> PyResult<(String, Option<String>, Option<Py<PyAny>>)> {
        match self
            .inner
            .read_combined(timeout.map(Duration::from_secs_f64))
        {
            ReadStatus::Line(StreamEvent { stream, line }) => Ok((
                "line".into(),
                Some(stream.as_str().into()),
                Some(self.decode_line(py, &line)?),
            )),
            ReadStatus::Timeout => Ok(("timeout".into(), None, None)),
            ReadStatus::Eof => Ok(("eof".into(), None, None)),
        }
    }

    #[pyo3(signature = (stream, timeout=None))]
    pub(crate) fn take_stream_line(
        &self,
        py: Python<'_>,
        stream: &str,
        timeout: Option<f64>,
    ) -> PyResult<(String, Option<Py<PyAny>>)> {
        match self
            .inner
            .read_stream(stream_kind(stream)?, timeout.map(Duration::from_secs_f64))
        {
            ReadStatus::Line(line) => Ok(("line".into(), Some(self.decode_line(py, &line)?))),
            ReadStatus::Timeout => Ok(("timeout".into(), None)),
            ReadStatus::Eof => Ok(("eof".into(), None)),
        }
    }

    pub(crate) fn captured_stdout(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        self.inner
            .captured_stdout()
            .into_iter()
            .map(|line| self.decode_line(py, &line))
            .collect()
    }

    pub(crate) fn captured_stderr(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        self.inner
            .captured_stderr()
            .into_iter()
            .map(|line| self.decode_line(py, &line))
            .collect()
    }

    pub(crate) fn captured_combined(&self, py: Python<'_>) -> PyResult<Vec<(String, Py<PyAny>)>> {
        self.inner
            .captured_combined()
            .into_iter()
            .map(|event| {
                Ok((
                    event.stream.as_str().to_string(),
                    self.decode_line(py, &event.line)?,
                ))
            })
            .collect()
    }

    pub(crate) fn captured_stream_bytes(&self, stream: &str) -> PyResult<usize> {
        Ok(self.inner.captured_stream_bytes(stream_kind(stream)?))
    }

    pub(crate) fn captured_combined_bytes(&self) -> usize {
        self.inner.captured_combined_bytes()
    }

    pub(crate) fn clear_captured_stream(&self, stream: &str) -> PyResult<usize> {
        Ok(self.inner.clear_captured_stream(stream_kind(stream)?))
    }

    pub(crate) fn clear_captured_combined(&self) -> usize {
        self.inner.clear_captured_combined()
    }

    #[pyo3(signature = (stream, pattern, is_regex=false, timeout=None))]
    pub(crate) fn expect(
        &self,
        py: Python<'_>,
        stream: &str,
        pattern: &str,
        is_regex: bool,
        timeout: Option<f64>,
    ) -> PyResult<ExpectResult> {
        let stream_kind = if stream == "combined" {
            None
        } else {
            Some(stream_kind(stream)?)
        };
        let mut buffer = match stream_kind {
            Some(kind) => self.captured_stream_text(py, kind)?,
            None => self.captured_combined_text(py)?,
        };
        let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs_f64(secs));
        let compiled_regex = if is_regex {
            Some(Regex::new(pattern).map_err(to_py_err)?)
        } else {
            None
        };

        loop {
            if let Some((matched, start, end, groups)) =
                self.find_expect_match(&buffer, pattern, compiled_regex.as_ref())?
            {
                return Ok((
                    "match".to_string(),
                    buffer,
                    Some(matched),
                    Some(start),
                    Some(end),
                    groups,
                ));
            }

            let wait_timeout = deadline.map(|limit| {
                let now = Instant::now();
                if now >= limit {
                    Duration::from_secs(0)
                } else {
                    limit
                        .saturating_duration_since(now)
                        .min(Duration::from_millis(100))
                }
            });
            if deadline.is_some_and(|limit| Instant::now() >= limit) {
                return Ok(("timeout".to_string(), buffer, None, None, None, Vec::new()));
            }

            match self.read_status_text(stream_kind, wait_timeout)? {
                ReadStatus::Line(line) => {
                    let decoded = self.decode_line_to_string(py, &line)?;
                    buffer.push_str(&decoded);
                    buffer.push('\n');
                }
                ReadStatus::Timeout => {
                    // Keep polling until the overall expect deadline expires.
                    continue;
                }
                ReadStatus::Eof => {
                    return Ok(("eof".to_string(), buffer, None, None, None, Vec::new()));
                }
            }
        }
    }

    #[staticmethod]
    pub(crate) fn is_pty_available() -> bool {
        false
    }
}

impl NativeRunningProcess {
    pub(crate) fn start_impl(&self) -> PyResult<()> {
        running_process::rp_rust_debug_scope!("running_process_py::NativeRunningProcess::start");
        self.inner.start().map_err(to_py_err)
    }

    pub(crate) fn wait_impl(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<i32> {
        running_process::rp_rust_debug_scope!("running_process_py::NativeRunningProcess::wait");
        py.detach(|| {
            self.inner
                .wait(timeout.map(Duration::from_secs_f64))
                .map_err(process_err_to_py)
        })
    }

    pub(crate) fn kill_impl(&self) -> PyResult<()> {
        running_process::rp_rust_debug_scope!("running_process_py::NativeRunningProcess::kill");
        self.inner.kill().map_err(to_py_err)
    }

    pub(crate) fn terminate_impl(&self) -> PyResult<()> {
        running_process::rp_rust_debug_scope!(
            "running_process_py::NativeRunningProcess::terminate"
        );
        self.inner.terminate().map_err(to_py_err)
    }

    pub(crate) fn close_impl(&self, py: Python<'_>) -> PyResult<()> {
        running_process::rp_rust_debug_scope!("running_process_py::NativeRunningProcess::close");
        py.detach(|| self.inner.close().map_err(process_err_to_py))
    }

    pub(crate) fn send_interrupt_impl(&self) -> PyResult<()> {
        running_process::rp_rust_debug_scope!(
            "running_process_py::NativeRunningProcess::send_interrupt"
        );
        let pid = self
            .inner
            .pid()
            .ok_or_else(|| PyRuntimeError::new_err("process is not running"))?;

        #[cfg(windows)]
        {
            public_symbols::rp_windows_generate_console_ctrl_break_public(pid, self.creationflags)
        }

        #[cfg(unix)]
        {
            if self.create_process_group {
                unix_signal_process_group(pid as i32, UnixSignal::Interrupt).map_err(to_py_err)?;
            } else {
                unix_signal_process(pid, UnixSignal::Interrupt).map_err(to_py_err)?;
            }
            Ok(())
        }
    }

    pub(crate) fn decode_line_to_string(&self, py: Python<'_>, line: &[u8]) -> PyResult<String> {
        if !self.text {
            return Ok(String::from_utf8_lossy(line).into_owned());
        }
        let encoding = self.encoding.as_deref().unwrap_or("utf-8");
        let errors = self.errors.as_deref().unwrap_or("replace");
        if encoding == "utf-8" && errors == "replace" {
            return Ok(String::from_utf8_lossy(line).into_owned());
        }
        PyBytes::new(py, line)
            .call_method1("decode", (encoding, errors))?
            .extract()
    }

    pub(crate) fn captured_stream_text(
        &self,
        py: Python<'_>,
        stream: StreamKind,
    ) -> PyResult<String> {
        let lines = match stream {
            StreamKind::Stdout => self.inner.captured_stdout(),
            StreamKind::Stderr => self.inner.captured_stderr(),
        };
        let mut text = String::new();
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                text.push('\n');
            }
            text.push_str(&self.decode_line_to_string(py, line)?);
        }
        Ok(text)
    }

    pub(crate) fn captured_combined_text(&self, py: Python<'_>) -> PyResult<String> {
        let lines = self.inner.captured_combined();
        let mut text = String::new();
        for (index, event) in lines.iter().enumerate() {
            if index > 0 {
                text.push('\n');
            }
            text.push_str(&self.decode_line_to_string(py, &event.line)?);
        }
        Ok(text)
    }

    pub(crate) fn read_status_text(
        &self,
        stream: Option<StreamKind>,
        timeout: Option<Duration>,
    ) -> PyResult<ReadStatus<Vec<u8>>> {
        Ok(match stream {
            Some(kind) => self.inner.read_stream(kind, timeout),
            None => match self.inner.read_combined(timeout) {
                ReadStatus::Line(StreamEvent { line, .. }) => ReadStatus::Line(line),
                ReadStatus::Timeout => ReadStatus::Timeout,
                ReadStatus::Eof => ReadStatus::Eof,
            },
        })
    }

    pub(crate) fn find_expect_match(
        &self,
        buffer: &str,
        pattern: &str,
        compiled_regex: Option<&Regex>,
    ) -> PyResult<Option<ExpectDetails>> {
        if compiled_regex.is_none() {
            // Literal string match
            let Some(start) = buffer.find(pattern) else {
                return Ok(None);
            };
            return Ok(Some((
                pattern.to_string(),
                start,
                start + pattern.len(),
                Vec::new(),
            )));
        }

        let regex = compiled_regex.unwrap();
        let Some(captures) = regex.captures(buffer) else {
            return Ok(None);
        };
        let whole = captures
            .get(0)
            .ok_or_else(|| PyRuntimeError::new_err("regex capture missing group 0"))?;
        let groups = captures
            .iter()
            .skip(1)
            .map(|group| {
                group
                    .map(|value| value.as_str().to_string())
                    .unwrap_or_default()
            })
            .collect();
        Ok(Some((
            whole.as_str().to_string(),
            whole.start(),
            whole.end(),
            groups,
        )))
    }

    pub(crate) fn decode_line(&self, py: Python<'_>, line: &[u8]) -> PyResult<Py<PyAny>> {
        if !self.text {
            return Ok(PyBytes::new(py, line).into_any().unbind());
        }
        let encoding = self.encoding.as_deref().unwrap_or("utf-8");
        let errors = self.errors.as_deref().unwrap_or("replace");
        if encoding == "utf-8" && errors == "replace" {
            let s = String::from_utf8_lossy(line);
            return Ok(PyString::new(py, &s).into_any().unbind());
        }
        Ok(PyBytes::new(py, line)
            .call_method1("decode", (encoding, errors))?
            .into_any()
            .unbind())
    }
}
