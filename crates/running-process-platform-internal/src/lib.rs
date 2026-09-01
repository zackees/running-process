//! Blessed asynchronous process operations.
//!
//! This crate is intentionally published as an implementation detail. It is
//! the only production owner of the Tokio process primitives used by the
//! async process API. Higher layers receive typed operations and never name
//! `tokio::process::Command` directly.

use std::cfg_select;
#[cfg(feature = "async-process")]
use std::ffi::{OsStr, OsString};
#[cfg(feature = "async-process")]
use std::io;
#[cfg(feature = "async-process")]
use std::path::PathBuf;
#[cfg(feature = "async-process")]
use std::process::{ExitStatus, Output, Stdio};

#[cfg(feature = "async-process")]
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
#[cfg(feature = "async-process")]
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

/// Neutral capability indexes for the eventual workspace-wide host boundary.
///
/// The indexes intentionally expose no operations yet: phase 2 establishes
/// ownership names before later phases move a capability behind them.
pub mod platform;

/// Temporary source-compatibility re-export for the pre-boundary PTY API.
///
/// New code must use [`platform::terminal`] facade-owned types. This root-only
/// alias deliberately stays outside the neutral facade and can be removed in
/// the next major release after downstream users have migrated.
#[cfg(feature = "pty")]
#[doc(hidden)]
pub use portable_pty as portable_pty_compat;

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
    assign_child_to_windows_job, cancel_capture_reader, canonical_environment_pairs,
    capture_reader_done, compat_shell_command, configure_exact_trace, configure_process_command,
    configure_process_command_for_bounded_owner_death, configure_sync_contained_command,
    configure_sync_daemon_command, configure_sync_daemon_command_with_inheritance,
    configure_trampoline_command, current_executable_build_id, exact_trace_capability, exit_code,
    monitor_console_windows, parent_has_console, prepare_capture_reader, set_process_name,
    set_window_icon_impl, shell_command, soft_terminate_process_group, spawn_sync,
    spawn_sync_daemon, spawn_sync_daemon_with_inheritance, start_descendant_monitor,
    start_exact_trace, sync_child_native_handle, trampoline_exit_code,
    unix_mark_extra_fds_close_on_exec, unix_set_priority, unix_signal_process,
    unix_signal_process_group, unix_signal_raw, window_icon_support_impl, CaptureCancellation,
    TracedChild, WindowsJobHandle,
};

#[cfg(feature = "terminal-graphics")]
pub use platform_imp::active_graphics_probe;

#[cfg(feature = "async-process")]
pub(crate) use platform_imp::{
    async_child_cpu_time, async_child_identity, signal_async_child, signal_async_child_group,
    AsyncChildIdentity,
};

#[cfg(feature = "process-inspection")]
pub use platform_imp::{kill_tree, process_snapshot, process_snapshot_for_pid};

pub use platform_imp::{autostart_register, autostart_render_registration, autostart_unregister};

pub use platform_imp::{process_install_owner_death_cleanup, process_owner_death_cleanup_target};

pub use platform_imp::process_install_shutdown_request_handler;

pub use platform_imp::fs_write_all_to_descriptor;

pub use platform_imp::{process_can_replace_current_image, process_replace_current_image};

pub use platform_imp::{
    process_executable_path, process_force_kill, process_same_executable_path,
    process_signal_terminate, ProcessLiveness,
};

pub use platform_imp::{
    resources_fd_exhaustion_error, resources_inode_capacity, resources_signals_fd_exhaustion,
    resources_signals_storage_exhaustion, resources_storage_exhaustion_error,
};

pub use platform_imp::{
    executable_file_name, executable_sibling_of_current_image, EXECUTABLE_EXTENSION,
};

#[cfg(feature = "fs")]
pub use platform_imp::{
    fs_create_private_file, fs_decode_path_bytes, fs_encode_path_bytes, fs_file_identity,
    fs_is_lock_conflict, fs_open_lock_file, fs_path_identity, fs_replace_file, fs_sync_directory,
    fs_try_lock_exclusive, fs_unlock, fs_user_config_dir, fs_user_data_dir, fs_user_run_data_root,
    fs_user_runtime_dir, fs_user_state_dir, FsFileIdentity,
};

pub use platform_imp::{
    host_boot_id, host_current_process_privilege, host_environment_keys_are_case_insensitive,
    host_filesystem_device_id, host_hostname, host_login_environment, host_machine_id,
    host_namespace_id, host_user_machine_identity, HostPrivilegedIdentity,
};

pub use platform_imp::host_login_environment_block;

pub use platform_imp::terminal_input;

#[cfg(feature = "ipc")]
pub use platform_imp::{
    ipc_broker_endpoint_name as IpcBrokerEndpointName, ipc_broker_v1_endpoint_path,
    ipc_broker_v2_runtime_dir, ipc_current_user_id, ipc_endpoint_is_filesystem_backed,
    ipc_endpoint_name_limit, ipc_endpoint_scope_bytes, ipc_nonblocking_zero_read_is_pending,
    ipc_select_endpoint_address, IpcEndpoint, IpcInheritedListener, IpcListener,
    IpcListenerNonblockingMode, IpcPeerIdentity, IpcPeerIdentitySource, IpcStream,
};

#[cfg(feature = "private-dir")]
pub use platform_imp::{
    private_dir_ensure_owner_private_directory, private_dir_owner_private_directory,
};

// Retain the implementation-detail root aliases selected by the historical
// `ipc` capability.  New callers use `platform::private_dir` instead.
#[cfg(feature = "ipc")]
pub use platform_imp::{
    private_dir_ensure_owner_private_directory as ipc_ensure_owner_private_directory,
    private_dir_owner_private_directory as ipc_owner_private_directory,
};

/// Failure details for the deprecated 4.x raw descriptor/handle handoff API.
///
/// This type exists only at the crate-root compatibility boundary. New product
/// mechanics use opaque [`platform::ipc::Stream`] operations instead.
#[cfg(feature = "ipc")]
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyHandoffError {
    kind: platform::ipc::HandoffTransferErrorKind,
    raw_os_error: Option<i32>,
    transferred_bytes: Option<usize>,
    expected_bytes: Option<usize>,
    detail: Option<String>,
}

#[cfg(feature = "ipc")]
impl LegacyHandoffError {
    pub(crate) fn new(
        kind: platform::ipc::HandoffTransferErrorKind,
        raw_os_error: Option<i32>,
    ) -> Self {
        Self {
            kind,
            raw_os_error,
            transferred_bytes: None,
            expected_bytes: None,
            detail: None,
        }
    }

    pub(crate) fn with_detail(
        kind: platform::ipc::HandoffTransferErrorKind,
        raw_os_error: Option<i32>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            raw_os_error,
            transferred_bytes: None,
            expected_bytes: None,
            detail: Some(detail.into()),
        }
    }

    #[doc(hidden)]
    pub fn partial(transferred_bytes: usize, expected_bytes: usize) -> Self {
        Self {
            kind: platform::ipc::HandoffTransferErrorKind::Failed,
            raw_os_error: None,
            transferred_bytes: Some(transferred_bytes),
            expected_bytes: Some(expected_bytes),
            detail: Some(format!(
                "SCM_RIGHTS connection transfer was partial ({transferred_bytes}/{expected_bytes} bytes)"
            )),
        }
    }

    /// Return the policy-neutral failure category.
    pub fn kind(&self) -> platform::ipc::HandoffTransferErrorKind {
        self.kind
    }

    /// Return the native error code retained for legacy public diagnostics.
    pub fn raw_os_error(&self) -> Option<i32> {
        self.raw_os_error
    }

    /// Return a partial payload count when the descriptor may have transferred.
    pub fn partial_counts(&self) -> Option<(usize, usize)> {
        self.transferred_bytes.zip(self.expected_bytes)
    }

    pub(crate) fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

/// Whether the deprecated 4.x SCM_RIGHTS compatibility transport is available.
#[cfg(feature = "ipc")]
#[doc(hidden)]
pub const LEGACY_SCM_RIGHTS_TRANSPORT_SUPPORTED: bool =
    platform_imp::LEGACY_SCM_RIGHTS_TRANSPORT_SUPPORTED;

/// Whether the deprecated 4.x DuplicateHandle compatibility transport is available.
#[cfg(feature = "ipc")]
#[doc(hidden)]
pub const LEGACY_DUPLICATE_HANDLE_TRANSPORT_SUPPORTED: bool =
    platform_imp::LEGACY_DUPLICATE_HANDLE_TRANSPORT_SUPPORTED;

/// Root-only adapter for the deprecated raw-descriptor handoff API.
#[cfg(feature = "ipc")]
#[doc(hidden)]
pub fn legacy_send_fd_to(
    socket: &std::path::Path,
    sent_fd: i32,
    payload: &[u8],
) -> Result<(), LegacyHandoffError> {
    platform_imp::legacy_send_fd_to(socket, sent_fd, payload)
}

/// Root-only adapter for the deprecated connected raw-descriptor handoff API.
#[cfg(feature = "ipc")]
#[doc(hidden)]
pub fn legacy_send_fd_over(
    socket_fd: i32,
    sent_fd: i32,
    payload: &[u8],
) -> Result<(), LegacyHandoffError> {
    platform_imp::legacy_send_fd_over(socket_fd, sent_fd, payload)
}

/// Root-only adapter for the deprecated raw-handle duplication API.
#[cfg(feature = "ipc")]
#[doc(hidden)]
pub fn legacy_duplicate_handle(
    source_handle: usize,
    backend_pid: u32,
) -> Result<usize, LegacyHandoffError> {
    platform_imp::legacy_duplicate_handle(source_handle, backend_pid)
}

/// Temporary source-compatibility conversion for public APIs that predate the
/// opaque IPC facade.
///
/// New code must keep [`IpcStream`] opaque. This root-only adapter exists so
/// `running-process` can preserve its established raw-stream callback contract
/// until the next major release without exposing the transport through
/// [`platform::ipc`].
#[cfg(feature = "ipc")]
#[doc(hidden)]
pub fn into_legacy_ipc_stream(stream: IpcStream) -> interprocess::local_socket::Stream {
    platform_imp::into_legacy_ipc_stream(stream)
}

/// Temporary source-compatibility conversion for legacy 4.x callback inputs.
#[cfg(feature = "ipc")]
#[doc(hidden)]
pub fn from_legacy_ipc_stream(stream: interprocess::local_socket::Stream) -> IpcStream {
    platform_imp::from_legacy_ipc_stream(stream)
}

/// Temporary source-compatibility conversion for public APIs that return an
/// `interprocess` endpoint name.
#[cfg(feature = "ipc")]
#[doc(hidden)]
pub fn legacy_ipc_name(path: &str) -> Result<interprocess::local_socket::Name<'_>, String> {
    platform_imp::legacy_ipc_name(path)
}

#[cfg(feature = "ipc-async")]
pub use platform_imp::{
    IpcAsyncListener, IpcAsyncStream, IpcIntoAsyncListener, IpcIntoAsyncStream,
};

#[cfg(feature = "pty")]
pub use platform_imp::terminal::{
    before_pty_spawn, current_backend_kind, find_child_processes, find_orphan_conhosts,
    input_payload, is_ignorable_process_control_error, prepare_unmanaged_pty_child,
    query_responses, resize_pty, shell_argv, signal_pty_tree, terminate_pty_child,
    wait_before_pty_close_supported, Backend, ChildProcessInfo, ConPtyBackendKind,
    OrphanConhostInfo, PtyProcessGuard, PtySpawnContext, TerminalInputSession,
};

#[cfg(feature = "session-relay")]
pub use platform_imp::relay_local_socket_session;

/// Apply host-owned setup for the legacy Tokio-command compatibility surface.
///
/// The public wrapper retains its policy type, while console suppression and
/// owner-death primitives stay inside the selected platform root.
#[cfg(feature = "async-process")]
pub fn configure_compat_tokio_command(
    command: &mut Command,
    show_console: bool,
    kill_when_owner_dies: bool,
) -> io::Result<()> {
    platform_imp::configure_compat_tokio_command(command, show_console, kill_when_owner_dies)
}

/// Complete host-owned setup after a legacy Tokio child has been spawned.
#[cfg(feature = "async-process")]
pub fn after_compat_tokio_spawn(child: &Child, kill_when_owner_dies: bool) -> io::Result<()> {
    platform_imp::after_compat_tokio_spawn(child, kill_when_owner_dies)
}

/// Stdio policy for one child stream.
#[cfg(feature = "async-process")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamMode {
    /// Leave the stream connected to the parent process.
    Inherit,
    /// Create an asynchronous pipe owned by the child handle.
    Piped,
    /// Connect the stream to the platform null device.
    Null,
}

#[cfg(feature = "async-process")]
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
#[cfg(feature = "async-process")]
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
    nice: Option<i32>,
}

#[cfg(feature = "async-process")]
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
            nice: None,
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
    /// Linux uses `PR_SET_PDEATHSIG(SIGTERM)` plus a pre-exec hard-exit race
    /// guard when the parent changed before that signal could be armed.
    /// Windows assigns the child to a
    /// process-wide kill-on-close Job Object. macOS forks a kqueue supervisor
    /// before exec and reports spawn success only after its owner and child
    /// watches are registered.
    pub fn kill_when_owner_dies(mut self, kill: bool) -> Self {
        self.kill_when_owner_dies = kill;
        self
    }

    /// Apply the host's existing niceness policy at child creation.
    ///
    /// On Unix this is the requested `setpriority(PRIO_PROCESS)` niceness.
    /// Windows maps the established niceness bands to process creation
    /// priority classes; it is deliberately a coarse host mapping rather
    /// than a claim that numeric nice values are portable.
    pub fn nice(mut self, nice: Option<i32>) -> Self {
        self.nice = nice;
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
            self.nice,
        )?;

        let child = command.spawn()?;
        platform_imp::after_spawn(&child, self.kill_when_owner_dies)?;
        Ok(PlatformChild::new(child, self.create_process_group))
    }
}

/// Owned child handle returned by [`SpawnSpec::spawn`].
#[cfg(feature = "async-process")]
pub struct PlatformChild {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    signal: PlatformEmergencySignal,
}

#[cfg(feature = "async-process")]
impl PlatformChild {
    fn new(mut child: Child, own_process_group: bool) -> Self {
        let signal = PlatformEmergencySignal {
            identity: async_child_identity(&child),
            own_process_group,
            // The legacy AsyncProcess actor historically retained only this
            // numeric child-group leader on macOS. Keep it launch-bound for
            // that API's compatibility path; sessions deliberately never use
            // it because their control capability promises identity safety.
            legacy_group_pid: child.id(),
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
#[cfg(feature = "async-process")]
pub struct PlatformLifecycle {
    child: Child,
}

#[cfg(feature = "async-process")]
impl PlatformLifecycle {
    /// Wait asynchronously for the child to exit.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    /// Request direct-child termination through the still-owned child handle
    /// without waiting for its reaping result.
    ///
    /// This is the identity-safe fallback when a host cannot provide a
    /// separately usable launch-bound signal capability (for example a Linux
    /// kernel without pidfds). The actor continues to own this lifecycle
    /// handle and performs the eventual reap itself.
    pub fn start_kill(&mut self) -> io::Result<()> {
        self.child.start_kill()
    }
}

/// Opaque, non-reap-capable emergency termination capability.
///
/// It can be used while the actor has a pending wait on
/// [`PlatformLifecycle`], but it cannot observe or consume the exit result.
#[cfg(feature = "async-process")]
pub struct PlatformEmergencySignal {
    identity: Option<AsyncChildIdentity>,
    own_process_group: bool,
    legacy_group_pid: Option<u32>,
}

#[cfg(feature = "async-process")]
impl PlatformEmergencySignal {
    /// Request immediate termination without waiting for process reaping.
    pub fn kill(&self) -> io::Result<()> {
        let Some(identity) = self.identity.as_ref() else {
            return Err(signal_target_unavailable());
        };
        signal_async_child(identity)
    }

    /// Ask the child's whole process group to shut down gracefully.
    ///
    /// Returns `Ok(false)` when the child was not spawned with
    /// [`SpawnSpec::create_process_group`]: there is no group to address, and
    /// signalling anyway would hit the caller's own group on POSIX or the
    /// caller's console on Windows. A missing or mismatched launch identity
    /// instead reports an unavailable target; it never falls back to a
    /// numeric group identifier that might have been reused.
    pub fn terminate_group_soft(&self) -> io::Result<bool> {
        if !self.own_process_group {
            return Ok(false);
        }
        let Some(identity) = self.identity.as_ref() else {
            return Err(signal_target_unavailable());
        };
        signal_async_child_group(identity).map(|()| true)
    }

    /// Legacy AsyncProcess-only graceful group termination.
    ///
    /// Most hosts retain an identity-safe asynchronous signal capability. On
    /// macOS there is no pidfd-equivalent for the Tokio child path, while the
    /// pre-session AsyncProcess contract historically sent SIGTERM to the
    /// launch child's numeric process group. Preserve that established
    /// best-effort behavior only for the legacy actor; sessions keep using
    /// [`Self::terminate_group_soft`] and therefore remain identity-safe.
    pub fn terminate_group_soft_legacy(&self) -> io::Result<bool> {
        if !self.own_process_group {
            return Ok(false);
        }
        if let Some(identity) = self.identity.as_ref() {
            return signal_async_child_group(identity).map(|()| true);
        }
        let pid = self
            .legacy_group_pid
            .ok_or_else(signal_target_unavailable)?;
        crate::platform::process::soft_terminate_process_group(pid).map(|()| true)
    }

    /// Return direct-child CPU time when this host can still verify the
    /// launch identity. Unsupported hosts and already-reused identities are
    /// reported as `None`, never as a PID-only best effort.
    pub fn cpu_time(&self) -> io::Result<Option<std::time::Duration>> {
        self.identity
            .as_ref()
            .map_or(Ok(None), async_child_cpu_time)
    }
}

#[cfg(feature = "async-process")]
fn signal_target_unavailable() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "child process launch identity is no longer available",
    )
}

/// Opaque piped stdin capability owned by a process actor.
#[cfg(feature = "async-process")]
pub struct PlatformStdin {
    stdin: ChildStdin,
}

#[cfg(feature = "async-process")]
impl PlatformStdin {
    /// Write and flush bytes to the child stdin pipe.
    pub async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stdin.write_all(bytes).await?;
        self.stdin.flush().await
    }
}

/// Opaque stdout or stderr reader owned by a process actor.
#[cfg(feature = "async-process")]
pub struct PlatformOutput {
    reader: OutputReader,
}

#[cfg(feature = "async-process")]
enum OutputReader {
    Stdout(ChildStdout),
    Stderr(ChildStderr),
}

#[cfg(feature = "async-process")]
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

#[cfg(feature = "async-process")]
fn stdin_not_piped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "child stdin is not piped")
}

#[cfg(feature = "async-process")]
fn stdout_not_piped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "child stdout is not piped")
}

#[cfg(feature = "async-process")]
fn stderr_not_piped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "child stderr is not piped")
}

#[cfg(feature = "async-process")]
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
#[cfg(feature = "async-process")]
pub fn shell_spec(command: impl AsRef<OsStr>) -> SpawnSpec {
    platform_imp::shell_spec(command.as_ref())
}

#[cfg(all(test, feature = "async-process"))]
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
