use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::backend::{Backend, PtyBackend, PtyChild, PtyMaster, PtySize, PtySlave};
use super::{
    is_ignorable_process_control_error, poll_pty_process, record_pty_input_metrics,
    spawn_pty_reader, store_pty_returncode, write_pty_input, IdleDetectorCore, NativePtyHandles,
    PtyError, PtyReadShared, PtyReadState,
};
#[cfg(unix)]
use super::posix_terminal_input_relay_worker;
#[cfg(windows)]
use super::{
    apply_windows_pty_priority, assign_child_to_windows_kill_on_close_job,
    assign_conpty_conhost_to_job, conhost_children_of_current_process,
};

#[cfg(unix)]
use super::pty_posix as pty_platform;
#[cfg(windows)]
use super::pty_windows;

pub struct NativePtyProcess {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub env: Option<Vec<(String, String)>>,
    pub rows: u16,
    pub cols: u16,
    #[cfg(windows)]
    pub nice: Option<i32>,
    pub handles: Arc<Mutex<Option<NativePtyHandles>>>,
    pub reader: Arc<PtyReadShared>,
    pub returncode: Arc<Mutex<Option<i32>>>,
    pub input_bytes_total: Arc<AtomicUsize>,
    pub newline_events_total: Arc<AtomicUsize>,
    pub submit_events_total: Arc<AtomicUsize>,
    /// When true, the reader thread writes PTY output to stdout.
    pub echo: Arc<AtomicBool>,
    /// When set, the reader thread feeds output directly to the idle detector.
    pub idle_detector: Arc<Mutex<Option<Arc<IdleDetectorCore>>>>,
    /// Visible (non-control) output bytes seen by the reader thread.
    pub output_bytes_total: Arc<AtomicUsize>,
    /// Control churn bytes (ANSI escapes, BS, CR, DEL) seen by the reader.
    pub control_churn_bytes_total: Arc<AtomicUsize>,
    pub reader_worker: Mutex<Option<thread::JoinHandle<()>>>,
    pub terminal_input_relay_stop: Arc<AtomicBool>,
    pub terminal_input_relay_active: Arc<AtomicBool>,
    pub terminal_input_relay_worker: Mutex<Option<thread::JoinHandle<()>>>,
}

pub(super) fn resolved_spawn_cwd(cwd: Option<&str>) -> Option<String> {
    cwd.map(str::to_owned).or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|cwd| cwd.to_string_lossy().to_string())
    })
}

impl NativePtyProcess {
    pub fn new(
        argv: Vec<String>,
        cwd: Option<String>,
        env: Option<Vec<(String, String)>>,
        rows: u16,
        cols: u16,
        nice: Option<i32>,
    ) -> Result<Self, PtyError> {
        if argv.is_empty() {
            return Err(PtyError::Other("command cannot be empty".into()));
        }
        #[cfg(not(windows))]
        let _ = nice;
        Ok(Self {
            argv,
            cwd,
            env,
            rows,
            cols,
            #[cfg(windows)]
            nice,
            handles: Arc::new(Mutex::new(None)),
            reader: Arc::new(PtyReadShared {
                state: Mutex::new(PtyReadState {
                    chunks: VecDeque::new(),
                    closed: false,
                }),
                condvar: Condvar::new(),
            }),
            returncode: Arc::new(Mutex::new(None)),
            input_bytes_total: Arc::new(AtomicUsize::new(0)),
            newline_events_total: Arc::new(AtomicUsize::new(0)),
            submit_events_total: Arc::new(AtomicUsize::new(0)),
            echo: Arc::new(AtomicBool::new(false)),
            idle_detector: Arc::new(Mutex::new(None)),
            output_bytes_total: Arc::new(AtomicUsize::new(0)),
            control_churn_bytes_total: Arc::new(AtomicUsize::new(0)),
            reader_worker: Mutex::new(None),
            terminal_input_relay_stop: Arc::new(AtomicBool::new(false)),
            terminal_input_relay_active: Arc::new(AtomicBool::new(false)),
            terminal_input_relay_worker: Mutex::new(None),
        })
    }

    pub fn mark_reader_closed(&self) {
        let mut guard = self.reader.state.lock().expect("pty read mutex poisoned");
        guard.closed = true;
        self.reader.condvar.notify_all();
    }

    pub fn store_returncode(&self, code: i32) {
        store_pty_returncode(&self.returncode, code);
    }

    pub(super) fn join_reader_worker(&self) {
        if let Some(worker) = self
            .reader_worker
            .lock()
            .expect("pty reader worker mutex poisoned")
            .take()
        {
            let _ = worker.join();
        }
    }

    pub fn record_input_metrics(&self, data: &[u8], submit: bool) {
        record_pty_input_metrics(
            &self.input_bytes_total,
            &self.newline_events_total,
            &self.submit_events_total,
            data,
            submit,
        );
    }

    pub fn write_impl(&self, data: &[u8], submit: bool) -> Result<(), PtyError> {
        self.record_input_metrics(data, submit);
        write_pty_input(&self.handles, data)?;
        Ok(())
    }

    pub fn request_terminal_input_relay_stop(&self) {
        self.terminal_input_relay_stop
            .store(true, Ordering::Release);
        self.terminal_input_relay_active
            .store(false, Ordering::Release);
    }

    pub fn start_terminal_input_relay_impl(&self) -> Result<(), PtyError> {
        let mut worker_guard = self
            .terminal_input_relay_worker
            .lock()
            .expect("pty terminal input relay mutex poisoned");
        if worker_guard.is_some() && self.terminal_input_relay_active() {
            return Ok(());
        }
        if self
            .handles
            .lock()
            .expect("pty handles mutex poisoned")
            .is_none()
        {
            return Err(PtyError::NotRunning);
        }

        self.terminal_input_relay_stop
            .store(false, Ordering::Release);
        self.terminal_input_relay_active
            .store(true, Ordering::Release);

        let handles = Arc::clone(&self.handles);
        let returncode = Arc::clone(&self.returncode);
        let input_bytes_total = Arc::clone(&self.input_bytes_total);
        let newline_events_total = Arc::clone(&self.newline_events_total);
        let submit_events_total = Arc::clone(&self.submit_events_total);
        let stop = Arc::clone(&self.terminal_input_relay_stop);
        let active = Arc::clone(&self.terminal_input_relay_active);

        #[cfg(windows)]
        {
            let capture = super::terminal_input::TerminalInputCore::new();
            capture.start_impl().map_err(PtyError::Io)?;
            *worker_guard = Some(thread::spawn(move || {
                loop {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    match poll_pty_process(&handles, &returncode) {
                        Ok(Some(_)) => break,
                        Ok(None) => {}
                        Err(_) => break,
                    }
                    match super::terminal_input::wait_for_terminal_input_event(
                        &capture.state,
                        &capture.condvar,
                        Some(Duration::from_millis(50)),
                    ) {
                        super::terminal_input::TerminalInputWaitOutcome::Event(event) => {
                            record_pty_input_metrics(
                                &input_bytes_total,
                                &newline_events_total,
                                &submit_events_total,
                                &event.data,
                                event.submit,
                            );
                            if write_pty_input(&handles, &event.data).is_err() {
                                break;
                            }
                        }
                        super::terminal_input::TerminalInputWaitOutcome::Timeout => continue,
                        super::terminal_input::TerminalInputWaitOutcome::Closed => break,
                    }
                }
                active.store(false, Ordering::Release);
                let _ = capture.stop_impl();
            }));
            Ok(())
        }

        #[cfg(unix)]
        {
            if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
                self.terminal_input_relay_active
                    .store(false, Ordering::Release);
                return Ok(());
            }

            *worker_guard = Some(thread::spawn(move || {
                posix_terminal_input_relay_worker(
                    handles,
                    returncode,
                    input_bytes_total,
                    newline_events_total,
                    submit_events_total,
                    stop,
                    active,
                );
            }));
            Ok(())
        }
    }

    pub fn stop_terminal_input_relay_impl(&self) {
        self.request_terminal_input_relay_stop();
        if let Some(worker) = self
            .terminal_input_relay_worker
            .lock()
            .expect("pty terminal input relay mutex poisoned")
            .take()
        {
            let _ = worker.join();
        }
    }

    pub fn terminal_input_relay_active(&self) -> bool {
        self.terminal_input_relay_active.load(Ordering::Acquire)
    }

    /// Synchronously tear down the PTY and reap the child.
    #[inline(never)]
    pub fn close_impl(&self) -> Result<(), PtyError> {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::close_impl");
        self.stop_terminal_input_relay_impl();
        let mut guard = self.handles.lock().expect("pty handles mutex poisoned");
        let Some(handles) = guard.take() else {
            self.mark_reader_closed();
            return Ok(());
        };
        drop(guard);

        #[cfg(windows)]
        let NativePtyHandles {
            master,
            writer,
            mut child,
            _job,
        } = handles;
        #[cfg(not(windows))]
        let NativePtyHandles {
            master,
            writer,
            mut child,
        } = handles;

        #[cfg(windows)]
        {
            {
                crate::rp_rust_debug_scope!(
                    "running_process::NativePtyProcess::close_impl.drop_job"
                );
                drop(_job);
            }

            {
                crate::rp_rust_debug_scope!(
                    "running_process::NativePtyProcess::close_impl.wait_job_exit"
                );
                let wait_deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            let code = status as i32;
                            self.store_returncode(code);
                            break;
                        }
                        Ok(None) if Instant::now() < wait_deadline => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Ok(None) => {
                            if let Err(err) = child.kill() {
                                if !is_ignorable_process_control_error(&err) {
                                    return Err(PtyError::Io(err));
                                }
                            }
                            let code = match child.wait() {
                                Ok(status) => status as i32,
                                Err(_) => -9,
                            };
                            self.store_returncode(code);
                            break;
                        }
                        Err(_) => {
                            self.store_returncode(-9);
                            break;
                        }
                    }
                }
            }
            {
                crate::rp_rust_debug_scope!(
                    "running_process::NativePtyProcess::close_impl.drop_writer"
                );
                drop(writer);
            }
            {
                crate::rp_rust_debug_scope!(
                    "running_process::NativePtyProcess::close_impl.drop_master"
                );
                drop(master);
            }
            drop(child);
            {
                crate::rp_rust_debug_scope!(
                    "running_process::NativePtyProcess::close_impl.join_reader"
                );
                self.join_reader_worker();
            }
            self.mark_reader_closed();
            Ok(())
        }

        #[cfg(not(windows))]
        {
            drop(writer);
            drop(master);

            let code = {
                crate::rp_rust_debug_scope!(
                    "running_process::NativePtyProcess::close_impl.wait_child"
                );
                match child.wait() {
                    Ok(status) => status as i32,
                    Err(_) => -9,
                }
            };
            drop(child);

            self.store_returncode(code);
            {
                crate::rp_rust_debug_scope!(
                    "running_process::NativePtyProcess::close_impl.join_reader"
                );
                self.join_reader_worker();
            }
            self.mark_reader_closed();
            Ok(())
        }
    }

    /// Best-effort, non-blocking teardown for use from `Drop`.
    #[inline(never)]
    pub fn close_nonblocking(&self) {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::close_nonblocking");
        #[cfg(windows)]
        self.request_terminal_input_relay_stop();
        let Ok(mut guard) = self.handles.lock() else {
            return;
        };
        let Some(handles) = guard.take() else {
            self.mark_reader_closed();
            return;
        };
        drop(guard);

        #[cfg(windows)]
        let NativePtyHandles {
            master,
            writer,
            mut child,
            _job,
        } = handles;
        #[cfg(not(windows))]
        let NativePtyHandles {
            master,
            writer,
            mut child,
        } = handles;

        if let Err(err) = child.kill() {
            if !is_ignorable_process_control_error(&err) {
                return;
            }
        }
        drop(writer);
        drop(master);
        drop(child);
        #[cfg(windows)]
        drop(_job);
        self.mark_reader_closed();
    }

    pub fn start_impl(&self) -> Result<(), PtyError> {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::start");
        let mut guard = self.handles.lock().expect("pty handles mutex poisoned");
        if guard.is_some() {
            return Err(PtyError::AlreadyStarted);
        }

        // Snapshot our conhost.exe children before openpty() so we can diff
        // after spawn to find the new conhost.exe created by ConPTY.
        #[cfg(windows)]
        let conhost_pids_before = conhost_children_of_current_process();

        let (mut master, slave) = Backend::openpty(PtySize {
            rows: self.rows,
            cols: self.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| PtyError::Spawn(e.to_string()))?;

        // Build argv/cwd/env in the shape the backend wants.
        let argv: Vec<std::ffi::OsString> =
            self.argv.iter().map(std::ffi::OsString::from).collect();
        let cwd = resolved_spawn_cwd(self.cwd.as_deref());
        let env: Option<Vec<(std::ffi::OsString, std::ffi::OsString)>> =
            self.env.as_ref().map(|e| {
                e.iter()
                    .map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v)))
                    .collect()
            });

        let reader = master
            .try_clone_reader()
            .map_err(|e| PtyError::Spawn(e.to_string()))?;
        let writer = master
            .take_writer()
            .map_err(|e| PtyError::Spawn(e.to_string()))?;
        let cwd_path = cwd.as_deref().map(std::path::Path::new);
        let child = slave
            .spawn(&argv, cwd_path, env.as_deref())
            .map_err(|e| PtyError::Spawn(e.to_string()))?;
        #[cfg(windows)]
        let job = assign_child_to_windows_kill_on_close_job(child.as_raw_handle())?;
        #[cfg(windows)]
        assign_conpty_conhost_to_job(&job, &conhost_pids_before);
        #[cfg(windows)]
        apply_windows_pty_priority(child.as_raw_handle(), self.nice)?;
        let shared = Arc::clone(&self.reader);
        let echo = Arc::clone(&self.echo);
        let idle_detector = Arc::clone(&self.idle_detector);
        let output_bytes = Arc::clone(&self.output_bytes_total);
        let churn_bytes = Arc::clone(&self.control_churn_bytes_total);
        let reader_worker = thread::spawn(move || {
            spawn_pty_reader(
                reader,
                shared,
                echo,
                idle_detector,
                output_bytes,
                churn_bytes,
            );
        });
        *self
            .reader_worker
            .lock()
            .expect("pty reader worker mutex poisoned") = Some(reader_worker);

        *guard = Some(NativePtyHandles {
            master: Box::new(master) as Box<dyn PtyMaster>,
            writer,
            child: Box::new(child) as Box<dyn PtyChild>,
            #[cfg(windows)]
            _job: job,
        });
        Ok(())
    }

    pub fn respond_to_queries_impl(&self, data: &[u8]) -> Result<(), PtyError> {
        #[cfg(windows)]
        {
            pty_windows::respond_to_queries(self, data)
        }

        #[cfg(unix)]
        {
            pty_platform::respond_to_queries(self, data)
        }
    }

    pub fn resize_impl(&self, rows: u16, cols: u16) -> Result<(), PtyError> {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::resize");
        let guard = self.handles.lock().expect("pty handles mutex poisoned");
        if let Some(handles) = guard.as_ref() {
            #[cfg(windows)]
            {
                let _ = (rows, cols, handles);
                // ConPTY resize can leave ClosePseudoConsole blocked during
                // teardown on Windows. Keep resize as a no-op until the
                // backend can cancel the outstanding PTY read safely.
                return Ok(());
            }

            #[cfg(not(windows))]
            handles
                .master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| PtyError::Other(e.to_string()))?;
        }
        Ok(())
    }

    pub fn send_interrupt_impl(&self) -> Result<(), PtyError> {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::send_interrupt");
        #[cfg(windows)]
        {
            pty_windows::send_interrupt(self)
        }

        #[cfg(unix)]
        {
            pty_platform::send_interrupt(self)
        }
    }

    pub fn wait_impl(&self, timeout: Option<f64>) -> Result<i32, PtyError> {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::wait");
        // Fast path: already exited.
        if let Some(code) = *self
            .returncode
            .lock()
            .expect("pty returncode mutex poisoned")
        {
            return Ok(code);
        }
        let start = Instant::now();
        loop {
            if let Some(code) = poll_pty_process(&self.handles, &self.returncode)? {
                return Ok(code);
            }
            if timeout.is_some_and(|limit| start.elapsed() >= Duration::from_secs_f64(limit)) {
                return Err(PtyError::Timeout);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn terminate_impl(&self) -> Result<(), PtyError> {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::terminate");
        #[cfg(windows)]
        {
            if self
                .handles
                .lock()
                .expect("pty handles mutex poisoned")
                .is_none()
            {
                return Err(PtyError::NotRunning);
            }
            self.close_impl()
        }

        #[cfg(unix)]
        {
            pty_platform::terminate(self)
        }
    }

    pub fn kill_impl(&self) -> Result<(), PtyError> {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::kill");
        #[cfg(windows)]
        {
            if self
                .handles
                .lock()
                .expect("pty handles mutex poisoned")
                .is_none()
            {
                return Err(PtyError::NotRunning);
            }
            self.close_impl()
        }

        #[cfg(unix)]
        {
            pty_platform::kill(self)
        }
    }

    pub fn terminate_tree_impl(&self) -> Result<(), PtyError> {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::terminate_tree");
        #[cfg(windows)]
        {
            pty_windows::terminate_tree(self)
        }

        #[cfg(unix)]
        {
            pty_platform::terminate_tree(self)
        }
    }

    pub fn kill_tree_impl(&self) -> Result<(), PtyError> {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::kill_tree");
        #[cfg(windows)]
        {
            pty_windows::kill_tree(self)
        }

        #[cfg(unix)]
        {
            pty_platform::kill_tree(self)
        }
    }

    /// Get the PID of the child process, if running.
    pub fn pid(&self) -> Result<Option<u32>, PtyError> {
        let guard = self.handles.lock().expect("pty handles mutex poisoned");
        if let Some(handles) = guard.as_ref() {
            #[cfg(unix)]
            if let Some(pid) = handles.master.process_group_leader() {
                if let Ok(pid) = u32::try_from(pid) {
                    return Ok(Some(pid));
                }
            }
            return Ok(Some(handles.child.pid()));
        }
        Ok(None)
    }

    /// Wait for a chunk of output from the PTY reader.
    /// Returns `Ok(Some(chunk))` on data, `Ok(None)` on timeout, `Err` on closed.
    pub fn read_chunk_impl(&self, timeout: Option<f64>) -> Result<Option<Vec<u8>>, PtyError> {
        let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs_f64(secs));
        let mut guard = self.reader.state.lock().expect("pty read mutex poisoned");
        loop {
            if let Some(chunk) = guard.chunks.pop_front() {
                return Ok(Some(chunk));
            }
            if guard.closed {
                return Err(PtyError::Other("Pseudo-terminal stream is closed".into()));
            }
            match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Ok(None); // timeout
                    }
                    let wait = deadline.saturating_duration_since(now);
                    let result = self
                        .reader
                        .condvar
                        .wait_timeout(guard, wait)
                        .expect("pty read mutex poisoned");
                    guard = result.0;
                }
                None => {
                    guard = self
                        .reader
                        .condvar
                        .wait(guard)
                        .expect("pty read mutex poisoned");
                }
            }
        }
    }

    /// Wait for the reader thread to close.
    pub fn wait_for_reader_closed_impl(&self, timeout: Option<f64>) -> bool {
        let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs_f64(secs));
        let mut guard = self.reader.state.lock().expect("pty read mutex poisoned");
        loop {
            if guard.closed {
                return true;
            }
            match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return false;
                    }
                    let wait = deadline.saturating_duration_since(now);
                    let result = self
                        .reader
                        .condvar
                        .wait_timeout(guard, wait)
                        .expect("pty read mutex poisoned");
                    guard = result.0;
                }
                None => {
                    guard = self
                        .reader
                        .condvar
                        .wait(guard)
                        .expect("pty read mutex poisoned");
                }
            }
        }
    }

    /// Wait for exit then drain remaining output.
    pub fn wait_and_drain_impl(
        &self,
        timeout: Option<f64>,
        drain_timeout: f64,
    ) -> Result<i32, PtyError> {
        let code = self.wait_impl(timeout)?;
        let deadline = Instant::now() + Duration::from_secs_f64(drain_timeout.max(0.0));
        let mut guard = self.reader.state.lock().expect("pty read mutex poisoned");
        while !guard.closed {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let result = self
                .reader
                .condvar
                .wait_timeout(guard, remaining)
                .expect("pty read mutex poisoned");
            guard = result.0;
        }
        Ok(code)
    }

    pub fn set_echo(&self, enabled: bool) {
        self.echo.store(enabled, Ordering::Release);
    }

    pub fn echo_enabled(&self) -> bool {
        self.echo.load(Ordering::Acquire)
    }

    pub fn attach_idle_detector(&self, detector: &Arc<IdleDetectorCore>) {
        let mut guard = self
            .idle_detector
            .lock()
            .expect("idle detector mutex poisoned");
        *guard = Some(Arc::clone(detector));
    }

    pub fn detach_idle_detector(&self) {
        let mut guard = self
            .idle_detector
            .lock()
            .expect("idle detector mutex poisoned");
        *guard = None;
    }

    pub fn pty_input_bytes_total(&self) -> usize {
        self.input_bytes_total.load(Ordering::Acquire)
    }

    pub fn pty_newline_events_total(&self) -> usize {
        self.newline_events_total.load(Ordering::Acquire)
    }

    pub fn pty_submit_events_total(&self) -> usize {
        self.submit_events_total.load(Ordering::Acquire)
    }

    pub fn pty_output_bytes_total(&self) -> usize {
        self.output_bytes_total.load(Ordering::Acquire)
    }

    pub fn pty_control_churn_bytes_total(&self) -> usize {
        self.control_churn_bytes_total.load(Ordering::Acquire)
    }
}

/// Safe defaults for a real interactive PTY session.
///
/// The helper turns on the parts that a terminal-style session usually needs:
/// output echo, terminal input relay, and automatic PTY query replies.
#[derive(Debug, Clone, Copy)]
pub struct InteractivePtyOptions {
    pub echo_output: bool,
    pub relay_terminal_input: bool,
    pub respond_to_queries: bool,
}

impl Default for InteractivePtyOptions {
    fn default() -> Self {
        Self {
            echo_output: true,
            relay_terminal_input: true,
            respond_to_queries: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct InteractivePtyPumpResult {
    pub chunks: Vec<Vec<u8>>,
    pub stream_closed: bool,
}

/// Canonical interactive PTY recipe for downstream Rust consumers.
///
/// `NativePtyProcess` remains the low-level primitive. This wrapper owns the
/// interactive setup that callers commonly forget to assemble correctly.
pub struct InteractivePtySession {
    process: NativePtyProcess,
    options: InteractivePtyOptions,
}

impl InteractivePtySession {
    pub fn new(process: NativePtyProcess) -> Self {
        Self::with_options(process, InteractivePtyOptions::default())
    }

    pub fn with_options(process: NativePtyProcess, options: InteractivePtyOptions) -> Self {
        Self { process, options }
    }

    pub fn process(&self) -> &NativePtyProcess {
        &self.process
    }

    pub fn start(&self) -> Result<(), PtyError> {
        self.process.set_echo(self.options.echo_output);
        self.process.start_impl()?;
        if self.options.relay_terminal_input {
            self.process.start_terminal_input_relay_impl()?;
        }
        Ok(())
    }

    pub fn pump_output(
        &self,
        timeout: Option<f64>,
        consume_all: bool,
    ) -> Result<InteractivePtyPumpResult, PtyError> {
        let mut pumped = InteractivePtyPumpResult::default();
        let mut next_timeout = timeout;
        loop {
            match self.process.read_chunk_impl(next_timeout) {
                Ok(Some(chunk)) => {
                    if self.options.respond_to_queries {
                        self.process.respond_to_queries_impl(&chunk)?;
                    }
                    pumped.chunks.push(chunk);
                    if !consume_all {
                        break;
                    }
                    next_timeout = Some(0.0);
                }
                Ok(None) => break,
                Err(PtyError::Other(message)) if message == "Pseudo-terminal stream is closed" => {
                    pumped.stream_closed = true;
                    break;
                }
                Err(err) => return Err(err),
            }
        }
        Ok(pumped)
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), PtyError> {
        self.process.resize_impl(rows, cols)
    }

    pub fn send_interrupt(&self) -> Result<(), PtyError> {
        self.process.send_interrupt_impl()
    }

    pub fn wait(&self, timeout: Option<f64>) -> Result<i32, PtyError> {
        self.process.wait_impl(timeout)
    }

    pub fn wait_and_drain(
        &self,
        timeout: Option<f64>,
        drain_timeout: f64,
    ) -> Result<i32, PtyError> {
        self.process.wait_and_drain_impl(timeout, drain_timeout)
    }

    pub fn terminate(&self) -> Result<(), PtyError> {
        self.process.terminate_impl()
    }

    pub fn kill(&self) -> Result<(), PtyError> {
        self.process.kill_impl()
    }

    pub fn close(&self) -> Result<(), PtyError> {
        self.process.close_impl()
    }
}

impl Drop for NativePtyProcess {
    fn drop(&mut self) {
        self.close_nonblocking();
    }
}
