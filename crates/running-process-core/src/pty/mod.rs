use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use thiserror::Error;

/// Re-exports for downstream crates that need portable-pty types.
pub mod reexports {
    pub use portable_pty;
}

#[cfg(unix)]
mod pty_posix;
#[cfg(windows)]
mod pty_windows;

pub mod terminal_input;

#[cfg(unix)]
use pty_posix as pty_platform;

#[derive(Debug, Error)]
pub enum PtyError {
    #[error("pseudo-terminal process already started")]
    AlreadyStarted,
    #[error("pseudo-terminal process is not running")]
    NotRunning,
    #[error("pseudo-terminal timed out")]
    Timeout,
    #[error("pseudo-terminal I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("pseudo-terminal spawn failed: {0}")]
    Spawn(String),
    #[error("pseudo-terminal error: {0}")]
    Other(String),
}

pub fn is_ignorable_process_control_error(err: &std::io::Error) -> bool {
    if matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput
    ) {
        return true;
    }
    #[cfg(unix)]
    if err.raw_os_error() == Some(libc::ESRCH) {
        return true;
    }
    false
}

pub struct PtyReadState {
    pub chunks: VecDeque<Vec<u8>>,
    pub closed: bool,
}

pub struct PtyReadShared {
    pub state: Mutex<PtyReadState>,
    pub condvar: Condvar,
}

pub struct NativePtyHandles {
    pub master: Box<dyn MasterPty + Send>,
    pub writer: Box<dyn Write + Send>,
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
    #[cfg(windows)]
    pub _job: WindowsJobHandle,
}

#[cfg(windows)]
pub struct WindowsJobHandle(pub usize);

#[cfg(windows)]
impl WindowsJobHandle {
    /// Assign an additional process (by PID) to this Job Object.
    pub fn assign_pid(&self, pid: u32) -> Result<(), std::io::Error> {
        use winapi::um::handleapi::CloseHandle;
        use winapi::um::processthreadsapi::OpenProcess;
        use winapi::um::winnt::PROCESS_SET_QUOTA;
        use winapi::um::winnt::PROCESS_TERMINATE;

        let handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let result = unsafe {
            winapi::um::jobapi2::AssignProcessToJobObject(
                self.0 as winapi::shared::ntdef::HANDLE,
                handle,
            )
        };
        unsafe { CloseHandle(handle) };
        if result == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for WindowsJobHandle {
    fn drop(&mut self) {
        unsafe {
            winapi::um::handleapi::CloseHandle(self.0 as winapi::shared::ntdef::HANDLE);
        }
    }
}

pub struct IdleMonitorState {
    pub last_reset_at: Instant,
    pub returncode: Option<i32>,
    pub interrupted: bool,
}

/// Core idle detection logic, shareable across threads via Arc.
/// The reader thread calls `record_output` directly.
pub struct IdleDetectorCore {
    pub timeout_seconds: f64,
    pub stability_window_seconds: f64,
    pub sample_interval_seconds: f64,
    pub reset_on_input: bool,
    pub reset_on_output: bool,
    pub count_control_churn_as_output: bool,
    pub enabled: Arc<AtomicBool>,
    pub state: Mutex<IdleMonitorState>,
    pub condvar: Condvar,
}

impl IdleDetectorCore {
    pub fn record_input(&self, byte_count: usize) {
        if !self.reset_on_input || byte_count == 0 {
            return;
        }
        let mut guard = self.state.lock().expect("idle monitor mutex poisoned");
        guard.last_reset_at = Instant::now();
        self.condvar.notify_all();
    }

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

    pub fn mark_exit(&self, returncode: i32, interrupted: bool) {
        let mut guard = self.state.lock().expect("idle monitor mutex poisoned");
        guard.returncode = Some(returncode);
        guard.interrupted = interrupted;
        self.condvar.notify_all();
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn set_enabled(&self, enabled: bool) {
        let was_enabled = self.enabled.swap(enabled, Ordering::AcqRel);
        if enabled && !was_enabled {
            let mut guard = self.state.lock().expect("idle monitor mutex poisoned");
            guard.last_reset_at = Instant::now();
        }
        self.condvar.notify_all();
    }

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

    fn join_reader_worker(&self) {
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
            let capture = terminal_input::TerminalInputCore::new();
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
                    match terminal_input::wait_for_terminal_input_event(
                        &capture.state,
                        &capture.condvar,
                        Some(Duration::from_millis(50)),
                    ) {
                        terminal_input::TerminalInputWaitOutcome::Event(event) => {
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
                        terminal_input::TerminalInputWaitOutcome::Timeout => continue,
                        terminal_input::TerminalInputWaitOutcome::Closed => break,
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
        crate::rp_rust_debug_scope!("running_process_core::NativePtyProcess::close_impl");
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
                    "running_process_core::NativePtyProcess::close_impl.drop_job"
                );
                drop(_job);
            }

            {
                crate::rp_rust_debug_scope!(
                    "running_process_core::NativePtyProcess::close_impl.wait_job_exit"
                );
                let wait_deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            let code = portable_exit_code(status);
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
                                Ok(status) => portable_exit_code(status),
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
                    "running_process_core::NativePtyProcess::close_impl.drop_writer"
                );
                drop(writer);
            }
            {
                crate::rp_rust_debug_scope!(
                    "running_process_core::NativePtyProcess::close_impl.drop_master"
                );
                drop(master);
            }
            drop(child);
            {
                crate::rp_rust_debug_scope!(
                    "running_process_core::NativePtyProcess::close_impl.join_reader"
                );
                self.join_reader_worker();
            }
            self.mark_reader_closed();
            return Ok(());
        }

        #[cfg(not(windows))]
        {
            drop(writer);
            drop(master);

            let code = {
                crate::rp_rust_debug_scope!(
                    "running_process_core::NativePtyProcess::close_impl.wait_child"
                );
                match child.wait() {
                    Ok(status) => portable_exit_code(status),
                    Err(_) => -9,
                }
            };
            drop(child);

            self.store_returncode(code);
            {
                crate::rp_rust_debug_scope!(
                    "running_process_core::NativePtyProcess::close_impl.join_reader"
                );
                self.join_reader_worker();
            }
            self.mark_reader_closed();
            return Ok(());
        }
    }

    /// Best-effort, non-blocking teardown for use from `Drop`.
    #[inline(never)]
    pub fn close_nonblocking(&self) {
        crate::rp_rust_debug_scope!("running_process_core::NativePtyProcess::close_nonblocking");
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
        crate::rp_rust_debug_scope!("running_process_core::NativePtyProcess::start");
        let mut guard = self.handles.lock().expect("pty handles mutex poisoned");
        if guard.is_some() {
            return Err(PtyError::AlreadyStarted);
        }

        // Snapshot our conhost.exe children before openpty() so we can diff
        // after spawn to find the new conhost.exe created by ConPTY.
        #[cfg(windows)]
        let conhost_pids_before = conhost_children_of_current_process();

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: self.rows,
                cols: self.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::Spawn(e.to_string()))?;

        let mut cmd = command_builder_from_argv(&self.argv);
        if let Some(cwd) = &self.cwd {
            cmd.cwd(cwd);
        }
        if let Some(env) = &self.env {
            cmd.env_clear();
            for (key, value) in env {
                cmd.env(key, value);
            }
        }

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| PtyError::Spawn(e.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| PtyError::Spawn(e.to_string()))?;
        let child = pair
            .slave
            .spawn_command(cmd)
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
            master: pair.master,
            writer,
            child,
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
        crate::rp_rust_debug_scope!("running_process_core::NativePtyProcess::resize");
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
        crate::rp_rust_debug_scope!("running_process_core::NativePtyProcess::send_interrupt");
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
        crate::rp_rust_debug_scope!("running_process_core::NativePtyProcess::wait");
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
        crate::rp_rust_debug_scope!("running_process_core::NativePtyProcess::terminate");
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
            return self.close_impl();
        }

        #[cfg(unix)]
        {
            pty_platform::terminate(self)
        }
    }

    pub fn kill_impl(&self) -> Result<(), PtyError> {
        crate::rp_rust_debug_scope!("running_process_core::NativePtyProcess::kill");
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
            return self.close_impl();
        }

        #[cfg(unix)]
        {
            pty_platform::kill(self)
        }
    }

    pub fn terminate_tree_impl(&self) -> Result<(), PtyError> {
        crate::rp_rust_debug_scope!("running_process_core::NativePtyProcess::terminate_tree");
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
        crate::rp_rust_debug_scope!("running_process_core::NativePtyProcess::kill_tree");
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
            return Ok(handles.child.process_id());
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

// ── Helper functions ──

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

pub fn command_builder_from_argv(argv: &[String]) -> CommandBuilder {
    let mut command = CommandBuilder::new(&argv[0]);
    if argv.len() > 1 {
        command.args(
            argv[1..]
                .iter()
                .map(OsString::from)
                .collect::<Vec<OsString>>(),
        );
    }
    command
}

#[inline(never)]
pub fn spawn_pty_reader(
    mut reader: Box<dyn Read + Send>,
    shared: Arc<PtyReadShared>,
    echo: Arc<AtomicBool>,
    idle_detector: Arc<Mutex<Option<Arc<IdleDetectorCore>>>>,
    output_bytes_total: Arc<AtomicUsize>,
    control_churn_bytes_total: Arc<AtomicUsize>,
) {
    crate::rp_rust_debug_scope!("running_process_core::spawn_pty_reader");
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

pub fn portable_exit_code(status: portable_pty::ExitStatus) -> i32 {
    if let Some(signal) = status.signal() {
        let signal = signal.to_ascii_lowercase();
        if signal.contains("interrupt") {
            return -2;
        }
        if signal.contains("terminated") {
            return -15;
        }
        if signal.contains("killed") {
            return -9;
        }
    }
    status.exit_code() as i32
}

pub fn input_contains_newline(data: &[u8]) -> bool {
    data.iter().any(|byte| matches!(*byte, b'\r' | b'\n'))
}

#[cfg(unix)]
struct PosixTerminalModeGuard {
    stdin_fd: i32,
    original_mode: libc::termios,
}

#[cfg(unix)]
impl Drop for PosixTerminalModeGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.stdin_fd, libc::TCSANOW, &self.original_mode);
        }
    }
}

#[cfg(unix)]
fn acquire_posix_terminal_mode_guard() -> Result<PosixTerminalModeGuard, std::io::Error> {
    let stdin_fd = libc::STDIN_FILENO;
    let mut original_mode = unsafe { std::mem::zeroed::<libc::termios>() };
    if unsafe { libc::tcgetattr(stdin_fd, &mut original_mode) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut raw_mode = original_mode;
    unsafe {
        libc::cfmakeraw(&mut raw_mode);
    }
    if unsafe { libc::tcsetattr(stdin_fd, libc::TCSANOW, &raw_mode) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(PosixTerminalModeGuard {
        stdin_fd,
        original_mode,
    })
}

#[cfg(unix)]
#[inline(never)]
fn posix_terminal_input_relay_worker(
    handles: Arc<Mutex<Option<NativePtyHandles>>>,
    returncode: Arc<Mutex<Option<i32>>>,
    input_bytes_total: Arc<AtomicUsize>,
    newline_events_total: Arc<AtomicUsize>,
    submit_events_total: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
) {
    let _terminal_guard = match acquire_posix_terminal_mode_guard() {
        Ok(guard) => guard,
        Err(_) => {
            active.store(false, Ordering::Release);
            return;
        }
    };

    let stdin_fd = libc::STDIN_FILENO;
    let mut buffer = vec![0_u8; 65536];
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        match poll_pty_process(&handles, &returncode) {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => break,
        }

        let mut pollfd = libc::pollfd {
            fd: stdin_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let poll_result = unsafe { libc::poll(&mut pollfd, 1, 50) };
        if poll_result < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if poll_result == 0 || pollfd.revents & libc::POLLIN == 0 {
            continue;
        }

        let read_result = unsafe { libc::read(stdin_fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read_result < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if read_result == 0 {
            continue;
        }

        let mut data = buffer[..read_result as usize].to_vec();
        loop {
            let mut drain_pollfd = libc::pollfd {
                fd: stdin_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let drain_ready = unsafe { libc::poll(&mut drain_pollfd, 1, 0) };
            if drain_ready <= 0 || drain_pollfd.revents & libc::POLLIN == 0 {
                break;
            }
            let drain_result =
                unsafe { libc::read(stdin_fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if drain_result <= 0 {
                break;
            }
            data.extend_from_slice(&buffer[..drain_result as usize]);
        }

        record_pty_input_metrics(
            &input_bytes_total,
            &newline_events_total,
            &submit_events_total,
            &data,
            input_contains_newline(&data),
        );
        if write_pty_input(&handles, &data).is_err() {
            break;
        }
    }

    active.store(false, Ordering::Release);
}

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

pub fn store_pty_returncode(returncode: &Arc<Mutex<Option<i32>>>, code: i32) {
    *returncode.lock().expect("pty returncode mutex poisoned") = Some(code);
}

pub fn poll_pty_process(
    handles: &Arc<Mutex<Option<NativePtyHandles>>>,
    returncode: &Arc<Mutex<Option<i32>>>,
) -> Result<Option<i32>, std::io::Error> {
    let mut guard = handles.lock().expect("pty handles mutex poisoned");
    let Some(handles) = guard.as_mut() else {
        return Ok(*returncode.lock().expect("pty returncode mutex poisoned"));
    };
    let status = handles.child.try_wait()?;
    let code = status.map(portable_exit_code);
    if let Some(code) = code {
        store_pty_returncode(returncode, code);
        return Ok(Some(code));
    }
    Ok(None)
}

pub fn write_pty_input(
    handles: &Arc<Mutex<Option<NativePtyHandles>>>,
    data: &[u8],
) -> Result<(), std::io::Error> {
    let mut guard = handles.lock().expect("pty handles mutex poisoned");
    let handles = guard.as_mut().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "Pseudo-terminal process is not running",
        )
    })?;
    #[cfg(windows)]
    let payload = pty_windows::input_payload(data);
    #[cfg(unix)]
    let payload = pty_platform::input_payload(data);
    handles.writer.write_all(&payload)?;
    handles.writer.flush()
}

#[cfg(windows)]
pub fn windows_terminal_input_payload(data: &[u8]) -> Vec<u8> {
    let mut translated = Vec::with_capacity(data.len());
    let mut index = 0usize;
    while index < data.len() {
        let current = data[index];
        if current == b'\r' {
            translated.push(current);
            if index + 1 < data.len() && data[index + 1] == b'\n' {
                translated.push(b'\n');
                index += 2;
                continue;
            }
            index += 1;
            continue;
        }
        if current == b'\n' {
            translated.push(b'\r');
            index += 1;
            continue;
        }
        translated.push(current);
        index += 1;
    }
    translated
}

#[cfg(windows)]
#[inline(never)]
pub fn assign_child_to_windows_kill_on_close_job(
    handle: Option<std::os::windows::io::RawHandle>,
) -> Result<WindowsJobHandle, PtyError> {
    crate::rp_rust_debug_scope!(
        "running_process_core::pty::assign_child_to_windows_kill_on_close_job"
    );
    use std::mem::zeroed;

    use winapi::shared::minwindef::FALSE;
    use winapi::um::handleapi::INVALID_HANDLE_VALUE;
    use winapi::um::jobapi2::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    };
    use winapi::um::winnt::{
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    let Some(handle) = handle else {
        return Err(PtyError::Other(
            "Pseudo-terminal child does not expose a Windows process handle".into(),
        ));
    };

    let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
    if job.is_null() || job == INVALID_HANDLE_VALUE {
        return Err(PtyError::Io(std::io::Error::last_os_error()));
    }

    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let result = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if result == FALSE {
        let err = std::io::Error::last_os_error();
        unsafe {
            winapi::um::handleapi::CloseHandle(job);
        }
        return Err(PtyError::Io(err));
    }

    let result = unsafe { AssignProcessToJobObject(job, handle.cast()) };
    if result == FALSE {
        let err = std::io::Error::last_os_error();
        unsafe {
            winapi::um::handleapi::CloseHandle(job);
        }
        return Err(PtyError::Io(err));
    }

    Ok(WindowsJobHandle(job as usize))
}

/// Information about a child process found via Toolhelp snapshot.
#[cfg(windows)]
#[derive(Debug, Clone)]
pub struct ChildProcessInfo {
    pub pid: u32,
    pub name: String,
}

/// Find all direct child processes of a given parent PID using the Windows Toolhelp API.
/// Returns PID and process name for each child.
#[cfg(windows)]
pub fn find_child_processes(parent_pid: u32) -> Vec<ChildProcessInfo> {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::tlhelp32::{
        CreateToolhelp32Snapshot, Process32First, Process32Next, PROCESSENTRY32, TH32CS_SNAPPROCESS,
    };

    let mut children = Vec::new();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == winapi::um::handleapi::INVALID_HANDLE_VALUE {
        return children;
    }

    let mut entry: PROCESSENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;

    if unsafe { Process32First(snapshot, &mut entry) } != 0 {
        loop {
            if entry.th32ParentProcessID == parent_pid {
                let name_bytes = &entry.szExeFile;
                let name_len = name_bytes
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(name_bytes.len());
                let name = String::from_utf8_lossy(
                    &name_bytes[..name_len]
                        .iter()
                        .map(|&c| c as u8)
                        .collect::<Vec<u8>>(),
                )
                .into_owned();
                children.push(ChildProcessInfo {
                    pid: entry.th32ProcessID,
                    name,
                });
            }
            if unsafe { Process32Next(snapshot, &mut entry) } == 0 {
                break;
            }
        }
    }

    unsafe { CloseHandle(snapshot) };
    children
}

/// Return PIDs of all conhost.exe processes that are children of the current process.
#[cfg(windows)]
fn conhost_children_of_current_process() -> Vec<u32> {
    let our_pid = std::process::id();
    find_child_processes(our_pid)
        .into_iter()
        .filter(|c| c.name.eq_ignore_ascii_case("conhost.exe"))
        .map(|c| c.pid)
        .collect()
}

/// After spawning a ConPTY child, find the new conhost.exe process that was created
/// by the ConPTY infrastructure (child of our process, not present in the "before"
/// snapshot) and assign it to the Job Object so it gets cleaned up on Job close.
#[cfg(windows)]
fn assign_conpty_conhost_to_job(job: &WindowsJobHandle, before_pids: &[u32]) {
    let after_pids = conhost_children_of_current_process();
    for pid in after_pids {
        if !before_pids.contains(&pid) {
            // This is a newly created conhost.exe — assign it to the Job.
            let _ = job.assign_pid(pid);
        }
    }
}

/// A conhost.exe process whose parent is no longer alive — likely an orphan
/// from a dead ConPTY session.
#[cfg(windows)]
#[derive(Debug, Clone)]
pub struct OrphanConhostInfo {
    /// PID of the orphaned conhost.exe.
    pub pid: u32,
    /// PID that was the parent when the snapshot was taken.
    pub parent_pid: u32,
    /// Name of the parent process, if it can be resolved (empty if parent is dead).
    pub parent_name: String,
}

/// Scan all conhost.exe processes on the system and return those whose parent
/// process is no longer alive. These are likely orphans from dead ConPTY sessions.
///
/// Uses `CreateToolhelp32Snapshot` for a point-in-time snapshot — no sysinfo
/// dependency, so it's lightweight and can be called frequently.
#[cfg(windows)]
pub fn find_orphan_conhosts() -> Vec<OrphanConhostInfo> {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::tlhelp32::{
        CreateToolhelp32Snapshot, Process32First, Process32Next, PROCESSENTRY32, TH32CS_SNAPPROCESS,
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == winapi::um::handleapi::INVALID_HANDLE_VALUE {
        return Vec::new();
    }

    let mut entry: PROCESSENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;

    // First pass: collect all PIDs and identify conhost.exe processes.
    let mut all_pids = std::collections::HashSet::new();
    let mut conhosts: Vec<(u32, u32)> = Vec::new(); // (pid, parent_pid)
    let mut parent_names: std::collections::HashMap<u32, String> = std::collections::HashMap::new();

    if unsafe { Process32First(snapshot, &mut entry) } != 0 {
        loop {
            let name_bytes = &entry.szExeFile;
            let name_len = name_bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(name_bytes.len());
            let name = String::from_utf8_lossy(
                &name_bytes[..name_len]
                    .iter()
                    .map(|&c| c as u8)
                    .collect::<Vec<u8>>(),
            )
            .into_owned();

            all_pids.insert(entry.th32ProcessID);
            parent_names.insert(entry.th32ProcessID, name.clone());

            if name.eq_ignore_ascii_case("conhost.exe") {
                conhosts.push((entry.th32ProcessID, entry.th32ParentProcessID));
            }

            if unsafe { Process32Next(snapshot, &mut entry) } == 0 {
                break;
            }
        }
    }

    unsafe { CloseHandle(snapshot) };

    // Second pass: filter to conhosts whose parent PID is not in the live set.
    conhosts
        .into_iter()
        .filter(|&(_, parent_pid)| !all_pids.contains(&parent_pid))
        .map(|(pid, parent_pid)| OrphanConhostInfo {
            pid,
            parent_pid,
            parent_name: parent_names.get(&parent_pid).cloned().unwrap_or_default(),
        })
        .collect()
}

#[cfg(windows)]
#[inline(never)]
pub fn apply_windows_pty_priority(
    handle: Option<std::os::windows::io::RawHandle>,
    nice: Option<i32>,
) -> Result<(), PtyError> {
    crate::rp_rust_debug_scope!("running_process_core::pty::apply_windows_pty_priority");
    use winapi::um::processthreadsapi::SetPriorityClass;
    use winapi::um::winbase::{
        ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS,
        IDLE_PRIORITY_CLASS,
    };

    let Some(handle) = handle else {
        return Ok(());
    };
    let flags = match nice {
        Some(value) if value >= 15 => IDLE_PRIORITY_CLASS,
        Some(value) if value >= 1 => BELOW_NORMAL_PRIORITY_CLASS,
        Some(value) if value <= -15 => HIGH_PRIORITY_CLASS,
        Some(value) if value <= -1 => ABOVE_NORMAL_PRIORITY_CLASS,
        _ => 0,
    };
    if flags == 0 {
        return Ok(());
    }
    let result = unsafe { SetPriorityClass(handle.cast(), flags) };
    if result == 0 {
        return Err(PtyError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}
