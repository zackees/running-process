//! Blessed asynchronous process operations.
//!
//! This crate is intentionally published as an implementation detail. It is
//! the only production owner of the Tokio process primitives used by the
//! async process API. Higher layers receive typed operations and never name
//! `tokio::process::Command` directly.

use std::cfg_select;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;
use std::process::{ExitStatus, Output, Stdio};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

/// Neutral capability indexes for the eventual workspace-wide host boundary.
///
/// The indexes intentionally expose no operations yet: phase 2 establishes
/// ownership names before later phases move a capability behind them.
pub mod platform;

// This is deliberately the crate's only host selector.  Facade modules are
// neutral; native details live behind the selected private root.
cfg_select! {
    target_os = "windows" => {
        mod platform_win;
        pub(crate) use platform_win as platform_imp;
    }
    target_os = "linux" => {
        mod platform_linux;
        pub(crate) use platform_linux as platform_imp;
    }
    target_os = "macos" => {
        mod platform_macos;
        pub(crate) use platform_macos as platform_imp;
    }
}

// Re-export the selected implementation once from this allowed host-selector
// root. Neutral capability facades re-export only crate-root names and never
// name the private `platform_imp` alias themselves.
pub use platform_imp::{
    active_graphics_probe, assign_child_to_windows_job, cancel_capture_reader,
    canonical_environment_pairs, capture_reader_done, compat_shell_command, configure_exact_trace,
    configure_process_command, configure_sync_contained_command, configure_sync_daemon_command,
    configure_trampoline_command, current_executable_build_id, exact_trace_capability, exit_code,
    kill_tree, monitor_console_windows, parent_has_console, prepare_capture_reader,
    process_snapshot, process_snapshot_for_pid, set_process_name, set_window_icon_impl,
    shell_command, soft_terminate_process_group, spawn_sync, spawn_sync_daemon,
    start_descendant_monitor, start_exact_trace, sync_child_native_handle, trampoline_exit_code,
    unix_mark_extra_fds_close_on_exec, unix_set_priority, unix_signal_process,
    unix_signal_process_group, unix_signal_raw, window_icon_support_impl, CaptureCancellation,
    TracedChild, WindowsJobHandle,
};

pub use platform_imp::terminal_input;

#[cfg(feature = "pty")]
pub use platform_imp::terminal::{
    before_pty_spawn, current_backend_kind, find_child_processes, find_orphan_conhosts,
    input_payload, is_ignorable_process_control_error, kill_pty_process_group, preferred_pty_pid,
    prepare_pty_child, query_responses, resize_pty, send_pty_interrupt, shell_argv,
    signal_pty_tree, terminate_pty_child, wait_before_pty_close_supported, Backend,
    ChildProcessInfo, ConPtyBackendKind, OrphanConhostInfo, PtyProcessGuard, PtySpawnContext,
    TerminalInputSession,
};

#[cfg(feature = "session-relay")]
pub use platform_imp::relay_local_socket_session;

/// Apply host-owned setup for the legacy Tokio-command compatibility surface.
///
/// The public wrapper retains its policy type, while console suppression and
/// owner-death primitives stay inside the selected platform root.
pub fn configure_compat_tokio_command(
    command: &mut Command,
    show_console: bool,
    kill_when_owner_dies: bool,
) -> io::Result<()> {
    platform_imp::configure_compat_tokio_command(command, show_console, kill_when_owner_dies)
}

/// Complete host-owned setup after a legacy Tokio child has been spawned.
pub fn after_compat_tokio_spawn(child: &Child, kill_when_owner_dies: bool) {
    platform_imp::after_compat_tokio_spawn(child, kill_when_owner_dies)
}

/// Stdio policy for one child stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamMode {
    /// Leave the stream connected to the parent process.
    Inherit,
    /// Create an asynchronous pipe owned by the child handle.
    Piped,
    /// Connect the stream to the platform null device.
    Null,
}

impl StreamMode {
    fn apply(self) -> Stdio {
        match self {
            Self::Inherit => Stdio::inherit(),
            Self::Piped => Stdio::piped(),
            Self::Null => Stdio::null(),
        }
    }
}

/// Typed spawn description accepted by the blessed process boundary.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    program: OsString,
    args: Vec<OsString>,
    current_dir: Option<PathBuf>,
    env: Vec<(OsString, OsString)>,
    clear_env: bool,
    stdin: StreamMode,
    stdout: StreamMode,
    stderr: StreamMode,
    create_process_group: bool,
    kill_when_owner_dies: bool,
}

impl SpawnSpec {
    /// Create a direct (non-shell) command description.
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            env: Vec::new(),
            clear_env: false,
            stdin: StreamMode::Inherit,
            stdout: StreamMode::Inherit,
            stderr: StreamMode::Inherit,
            create_process_group: false,
            kill_when_owner_dies: false,
        }
    }

    /// Append one argument without requiring UTF-8.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Set the child working directory.
    pub fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }

    /// Add an environment override.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Start with an empty inherited environment before applying overrides.
    pub fn clear_env(mut self, clear: bool) -> Self {
        self.clear_env = clear;
        self
    }

    /// Configure child stdin.
    pub fn stdin(mut self, mode: StreamMode) -> Self {
        self.stdin = mode;
        self
    }

    /// Configure child stdout.
    pub fn stdout(mut self, mode: StreamMode) -> Self {
        self.stdout = mode;
        self
    }

    /// Configure child stderr.
    pub fn stderr(mut self, mode: StreamMode) -> Self {
        self.stderr = mode;
        self
    }

    /// Put the child in its own process group.
    ///
    /// This is what makes a group-wide soft signal addressable at all:
    /// [`PlatformEmergencySignal::terminate_group_soft`] is a no-op without
    /// it, because on POSIX the negative-PID signal would otherwise reach the
    /// caller's own group, and on Windows `GenerateConsoleCtrlEvent` only
    /// routes to children spawned with `CREATE_NEW_PROCESS_GROUP`. It also
    /// detaches the child from the parent's console Ctrl+C, so it is opt-in.
    pub fn create_process_group(mut self, create: bool) -> Self {
        self.create_process_group = create;
        self
    }

    /// Kill this child when the spawning process exits unexpectedly.
    ///
    /// Linux uses `PR_SET_PDEATHSIG(SIGTERM)`. Windows assigns the child to a
    /// process-wide kill-on-close Job Object. macOS forks a kqueue supervisor
    /// before exec and reports spawn success only after its owner and child
    /// watches are registered.
    pub fn kill_when_owner_dies(mut self, kill: bool) -> Self {
        self.kill_when_owner_dies = kill;
        self
    }

    /// Spawn using the canonical asynchronous platform operation.
    pub async fn spawn(self) -> io::Result<PlatformChild> {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        if let Some(current_dir) = self.current_dir.as_deref() {
            command.current_dir(current_dir);
        }
        if self.clear_env {
            command.env_clear();
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
            .stdin(self.stdin.apply())
            .stdout(self.stdout.apply())
            .stderr(self.stderr.apply());
        platform_imp::configure_command(
            &mut command,
            self.create_process_group,
            self.kill_when_owner_dies,
        )?;

        let child = command.spawn()?;
        platform_imp::after_spawn(&child, self.kill_when_owner_dies);
        Ok(PlatformChild::new(child, self.create_process_group))
    }
}

/// Owned child handle returned by [`SpawnSpec::spawn`].
pub struct PlatformChild {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    signal: PlatformEmergencySignal,
}

impl PlatformChild {
    fn new(mut child: Child, own_process_group: bool) -> Self {
        let signal = PlatformEmergencySignal {
            pid: child.id(),
            own_process_group,
        };
        Self {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            child,
            signal,
        }
    }

    /// Return the operating-system process identifier, if available.
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Wait for completion without capturing output.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    /// Terminate the child and wait for its exit.
    pub async fn kill(&mut self) -> io::Result<()> {
        self.child.kill().await
    }

    /// Capture piped stdout and stderr while waiting for the child.
    pub async fn wait_with_output(self) -> io::Result<Output> {
        let Self {
            mut child,
            stdin,
            stdout,
            stderr,
            ..
        } = self;
        // Match Tokio's `Child::wait_with_output` contract: one-shot output
        // closes an owned stdin pipe so a child waiting for EOF can finish.
        drop(stdin);
        let (status, stdout, stderr) = tokio::try_join!(
            child.wait(),
            read_owned_to_end(stdout),
            read_owned_to_end(stderr),
        )?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    /// Write bytes to piped stdin and flush them.
    pub async fn write_stdin(&mut self, bytes: &[u8]) -> io::Result<()> {
        let stdin = self.stdin.as_mut().ok_or_else(stdin_not_piped)?;
        stdin.write_all(bytes).await?;
        stdin.flush().await
    }

    /// Close the piped stdin handle, delivering EOF to the child.
    ///
    /// This operation is idempotent. Closing an inherited or null stdin is
    /// also a no-op because there is no owned pipe to close.
    pub fn close_stdin(&mut self) {
        drop(self.stdin.take());
    }

    /// Read all bytes from piped stdout without waiting for process exit.
    pub async fn read_stdout_to_end(&mut self) -> io::Result<Vec<u8>> {
        let stdout = self.stdout.as_mut().ok_or_else(stdout_not_piped)?;
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await?;
        Ok(bytes)
    }

    /// Read all bytes from piped stderr without waiting for process exit.
    pub async fn read_stderr_to_end(&mut self) -> io::Result<Vec<u8>> {
        let stderr = self.stderr.as_mut().ok_or_else(stderr_not_piped)?;
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await?;
        Ok(bytes)
    }

    /// Split this child into sealed actor capabilities.
    ///
    /// The lifecycle wait handle, emergency termination handle, input pipe,
    /// and output readers are deliberately separate so the actor can keep
    /// accepting control commands while an asynchronous exit wait is pending.
    pub fn into_actor_parts(
        self,
    ) -> (
        PlatformLifecycle,
        PlatformEmergencySignal,
        Option<PlatformStdin>,
        Option<PlatformOutput>,
        Option<PlatformOutput>,
    ) {
        (
            PlatformLifecycle { child: self.child },
            self.signal,
            self.stdin.map(|stdin| PlatformStdin { stdin }),
            self.stdout.map(PlatformOutput::stdout),
            self.stderr.map(PlatformOutput::stderr),
        )
    }
}

/// Opaque exit-wait capability owned by a process actor.
pub struct PlatformLifecycle {
    child: Child,
}

impl PlatformLifecycle {
    /// Wait asynchronously for the child to exit.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }
}

/// Opaque, non-reap-capable emergency termination capability.
///
/// It can be used while the actor has a pending wait on
/// [`PlatformLifecycle`], but it cannot observe or consume the exit result.
pub struct PlatformEmergencySignal {
    pid: Option<u32>,
    own_process_group: bool,
}

impl PlatformEmergencySignal {
    /// Request immediate termination without waiting for process reaping.
    pub fn kill(&self) -> io::Result<()> {
        platform_imp::signal_process(self.target()?)
    }

    /// Ask the child's whole process group to shut down gracefully.
    ///
    /// Returns `Ok(false)` when the child was not spawned with
    /// [`SpawnSpec::create_process_group`]: there is no group to address, and
    /// signalling anyway would hit the caller's own group on POSIX or the
    /// caller's console on Windows. A child that has already exited is also
    /// `Ok` -- the soft step's only job is to give a live child a chance to
    /// clean up before a hard kill, so a dead target is a success.
    pub fn terminate_group_soft(&self) -> io::Result<bool> {
        if !self.own_process_group {
            return Ok(false);
        }
        platform_imp::signal_process_group(self.target()?).map(|()| true)
    }

    fn target(&self) -> io::Result<u32> {
        self.pid.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "child process no longer has an emergency signal target",
            )
        })
    }
}

/// Opaque piped stdin capability owned by a process actor.
pub struct PlatformStdin {
    stdin: ChildStdin,
}

impl PlatformStdin {
    /// Write and flush bytes to the child stdin pipe.
    pub async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stdin.write_all(bytes).await?;
        self.stdin.flush().await
    }
}

/// Opaque stdout or stderr reader owned by a process actor.
pub struct PlatformOutput {
    reader: OutputReader,
}

enum OutputReader {
    Stdout(ChildStdout),
    Stderr(ChildStderr),
}

impl PlatformOutput {
    fn stdout(stdout: ChildStdout) -> Self {
        Self {
            reader: OutputReader::Stdout(stdout),
        }
    }

    fn stderr(stderr: ChildStderr) -> Self {
        Self {
            reader: OutputReader::Stderr(stderr),
        }
    }

    /// Drain this output endpoint to EOF without blocking a runtime worker.
    pub async fn read_to_end(self) -> io::Result<Vec<u8>> {
        match self.reader {
            OutputReader::Stdout(stdout) => read_owned_to_end(Some(stdout)).await,
            OutputReader::Stderr(stderr) => read_owned_to_end(Some(stderr)).await,
        }
    }

    /// Read the next asynchronous chunk from this output endpoint.
    ///
    /// The caller owns the buffer and therefore controls the amount of data
    /// retained at each read. EOF is reported as `Ok(0)`.
    pub async fn read_chunk(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match &mut self.reader {
            OutputReader::Stdout(stdout) => stdout.read(buffer).await,
            OutputReader::Stderr(stderr) => stderr.read(buffer).await,
        }
    }
}

fn stdin_not_piped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "child stdin is not piped")
}

fn stdout_not_piped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "child stdout is not piped")
}

fn stderr_not_piped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "child stderr is not piped")
}

async fn read_owned_to_end<R>(reader: Option<R>) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let Some(mut reader) = reader else {
        return Ok(Vec::new());
    };
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

/// Build a shell command using the host platform's supported shell.
pub fn shell_spec(command: impl AsRef<OsStr>) -> SpawnSpec {
    platform_imp::shell_spec(command.as_ref())
}

#[cfg(test)]
mod tests {
    use super::{shell_spec, SpawnSpec, StreamMode};

    fn fixture_command() -> SpawnSpec {
        #[cfg(windows)]
        {
            shell_spec("echo async-platform-internal")
        }
        #[cfg(not(windows))]
        {
            shell_spec("printf async-platform-internal")
        }
    }

    #[tokio::test]
    async fn blessed_spawn_captures_output_without_sync_wait() {
        let output = fixture_command()
            .stdout(StreamMode::Piped)
            .stderr(StreamMode::Piped)
            .spawn()
            .await
            .expect("spawn")
            .wait_with_output()
            .await
            .expect("wait with output");

        assert!(output.status.success());
        let expected = if cfg!(windows) {
            b"async-platform-internal\r\n".as_slice()
        } else {
            b"async-platform-internal".as_slice()
        };
        assert_eq!(output.stdout, expected);
        assert!(output.stderr.is_empty());
    }

    #[tokio::test]
    async fn blessed_spawn_reports_missing_program() {
        let result = SpawnSpec::new("running-process-program-that-does-not-exist")
            .spawn()
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn one_shot_output_closes_owned_stdin() {
        #[cfg(windows)]
        let spec = shell_spec("more > nul & echo done");
        #[cfg(not(windows))]
        let spec = shell_spec("cat > /dev/null; printf done");

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            spec.stdin(StreamMode::Piped)
                .stdout(StreamMode::Piped)
                .stderr(StreamMode::Piped)
                .spawn()
                .await
                .expect("spawn")
                .wait_with_output(),
        )
        .await
        .expect("stdin is closed for one-shot output")
        .expect("output succeeds");

        let expected = if cfg!(windows) {
            b"done\r\n".as_slice()
        } else {
            b"done".as_slice()
        };
        assert_eq!(output.stdout, expected);
    }
}
