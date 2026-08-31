//! Process spawning, containment, inspection, termination, and stdio.

pub use crate::{
    assign_child_to_windows_job, cancel_capture_reader, canonical_environment_pairs,
    capture_reader_done, compat_shell_command, configure_exact_trace, configure_process_command,
    configure_sync_contained_command, configure_sync_daemon_command,
    configure_sync_daemon_command_with_inheritance, configure_trampoline_command,
    current_executable_build_id, exact_trace_capability, exit_code, monitor_console_windows,
    parent_has_console, prepare_capture_reader, set_process_name, shell_command,
    soft_terminate_process_group, spawn_sync, spawn_sync_daemon,
    spawn_sync_daemon_with_inheritance, start_descendant_monitor, start_exact_trace,
    sync_child_native_handle, trampoline_exit_code, unix_mark_extra_fds_close_on_exec,
    CaptureCancellation, TracedChild, WindowsJobHandle,
};

#[cfg(feature = "async-process")]
pub use crate::{
    PlatformChild, PlatformEmergencySignal, PlatformLifecycle, PlatformOutput, PlatformStdin,
    SpawnSpec, StreamMode,
};

#[cfg(feature = "process-inspection")]
pub use crate::{kill_tree, process_snapshot, process_snapshot_for_pid};

/// Host-neutral command options selected by the caller before spawning.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcessCommandConfig {
    pub creation_flags: Option<u32>,
    pub create_process_group: bool,
    pub nice: Option<i32>,
    pub address_space_limit_bytes: Option<u64>,
}

/// Opaque descriptor that a daemon spawn deliberately preserves through exec.
///
/// Normal daemon spawns retain the close-extra-descriptors default. The IPC
/// listener handoff creates this value only after preparing its listener, and
/// the Unix spawn boundary reopens exactly this descriptor after applying the
/// default close-on-exec sweep.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DaemonExecInheritance {
    descriptor: i32,
}

impl DaemonExecInheritance {
    // The token is constructed and consumed only by Unix IPC backends. Keep
    // its representation host-neutral here so platform selection stays in
    // those backend modules rather than leaking into the shared facade.
    #[allow(dead_code)]
    pub(crate) fn preserving_descriptor(descriptor: i32) -> Self {
        Self { descriptor }
    }

    #[allow(dead_code)]
    pub(crate) fn descriptor(self) -> i32 {
        self.descriptor
    }
}

/// Availability of an invasive, lossless launched-tree trace backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactTraceCapability {
    pub available: bool,
    pub backend: &'static str,
    pub reason: &'static str,
    pub non_invasive_backend: &'static str,
    pub non_invasive_grade: NonInvasiveObservationGrade,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NonInvasiveObservationGrade {
    KernelNotification,
    KernelHintReconciled,
    SnapshotInferred,
}

/// A raw, bounded spawning-thread capture collected while a tracee is stopped.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TraceOriginArtifact {
    pub origin_pid: u32,
    pub thread_id: u32,
    pub architecture: String,
    pub register_format: String,
    pub executable: Option<std::path::PathBuf>,
    pub registers: Vec<u8>,
    pub stack_pointer: Option<u64>,
    pub instruction_pointer: Option<u64>,
    pub stack: Vec<u8>,
    pub truncated: bool,
    pub module_map: Vec<u8>,
    pub module_map_truncated: bool,
}

/// Native launched-tree event produced by an exact trace backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactTraceEvent {
    pub sequence: u64,
    pub pid: u32,
    pub parent_pid: Option<u32>,
    pub parent_start_key: Option<u64>,
    pub start_key: Option<u64>,
    pub timestamp: std::time::SystemTime,
    pub kind: ExactTraceEventKind,
    pub executable: Option<std::path::PathBuf>,
    pub argv: Option<Vec<std::ffi::OsString>>,
    pub origin: Option<TraceOriginArtifact>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExactTraceEventKind {
    Spawn,
    Exec,
    Exit {
        exit_code: Option<i32>,
        signal: Option<i32>,
        raw_status: i64,
    },
    Loss {
        reason: String,
    },
}

/// A descendant lifecycle fact reported by the host monitor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DescendantEvent {
    Started {
        pid: u32,
        /// Immediate parent of the new descendant, when the discovery
        /// mechanism knows it: the Linux `children`-file walk and the
        /// macOS process-snapshot inversion both do; the Windows job
        /// IOCP notification is PID-only, so it reports `None` rather
        /// than paying a racy toolhelp scan per event.
        parent_pid: Option<u32>,
    },
    Exited(u32),
    /// The platform backend has completed its final reconciliation and no
    /// further descendant events can arrive.
    Completed,
}

/// Shared cancellation handle for a host-native descendant monitor.
pub struct DescendantMonitorStop {
    stopped: std::sync::atomic::AtomicBool,
    mutex: std::sync::Mutex<()>,
    wake: std::sync::Condvar,
}

impl DescendantMonitorStop {
    /// Create an untriggered monitor cancellation handle.
    pub fn new() -> Self {
        Self {
            stopped: std::sync::atomic::AtomicBool::new(false),
            mutex: std::sync::Mutex::new(()),
            wake: std::sync::Condvar::new(),
        }
    }

    /// Report whether monitoring was cancelled.
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Cancel monitoring and wake a sleeping monitor immediately.
    pub fn stop(&self) {
        let _guard = self.mutex.lock().unwrap_or_else(|error| error.into_inner());
        if !self.stopped.swap(true, std::sync::atomic::Ordering::AcqRel) {
            self.wake.notify_all();
        }
    }

    /// Wait until cancelled or `timeout` expires, returning whether cancelled.
    pub fn wait_timeout(&self, timeout: std::time::Duration) -> bool {
        if self.is_stopped() {
            return true;
        }
        let guard = self.mutex.lock().unwrap_or_else(|error| error.into_inner());
        if self.is_stopped() {
            return true;
        }
        let (_guard, _wait_result) = self
            .wake
            .wait_timeout(guard, timeout)
            .unwrap_or_else(|error| error.into_inner());
        self.is_stopped()
    }
}

impl Default for DescendantMonitorStop {
    fn default() -> Self {
        Self::new()
    }
}

/// Identifies one captured child output stream.
#[derive(Clone, Copy)]
pub enum CaptureStream {
    Stdout,
    Stderr,
}

/// Metadata about one visible window observed by console-popup monitoring.
#[derive(Debug, Clone)]
pub struct ConsoleWindowInfo {
    pub pid: u32,
    pub title: String,
    pub hwnd: u64,
}

/// A platform-owned identity record used when observing a process tree.
/// The timestamp fields are opaque host-native creation-time components and
/// must only be compared for equality.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessSnapshot {
    pub pid: u32,
    pub parent_pid: u32,
    pub start_time_a: u64,
    pub start_time_b: u64,
}

/// Environment base selected by the shared caller for a synchronous spawn.
///
/// Explicit `Command::env` additions and removals remain on the command and
/// are applied after this base by the selected platform implementation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncEnvironment {
    /// Start with the spawning process's ambient environment.
    Inherit,
    /// Start with this complete, caller-assembled base environment.
    Explicit(Vec<(std::ffi::OsString, std::ffi::OsString)>),
}

/// Caller-supplied stdio bindings for a contained synchronous child.
///
/// Each stream is independently configured. `drain_timeout` bounds how long
/// wrapper-owned pipe ends remain open after the child exits; `None` leaves
/// pipe closure entirely to the caller. `show_console` only affects Windows.
pub struct SpawnStdio<'a> {
    /// Child standard input source.
    pub stdin: StdioSource<'a>,
    /// Child standard output destination.
    pub stdout: StdioSource<'a>,
    /// Child standard error destination.
    pub stderr: StdioSource<'a>,
    /// Maximum post-exit pipe drain interval.
    pub drain_timeout: Option<std::time::Duration>,
    /// Whether a Windows child may inherit or allocate a visible console.
    pub show_console: bool,
}

impl Default for SpawnStdio<'_> {
    fn default() -> Self {
        Self {
            stdin: StdioSource::Null,
            stdout: StdioSource::Parent,
            stderr: StdioSource::Parent,
            drain_timeout: Some(std::time::Duration::from_secs(2)),
            show_console: false,
        }
    }
}

/// Caller-supplied output bindings for a detached synchronous child.
///
/// Detached children may write only to the platform null device or to a
/// caller-owned file. Parent stdio and anonymous pipes are intentionally not
/// available because either can retain or depend on the launching process.
pub struct DaemonStdio<'a> {
    /// Child standard output destination.
    pub stdout: DaemonStdioSource<'a>,
    /// Child standard error destination.
    pub stderr: DaemonStdioSource<'a>,
}

impl Default for DaemonStdio<'_> {
    fn default() -> Self {
        Self {
            stdout: DaemonStdioSource::Null,
            stderr: DaemonStdioSource::Null,
        }
    }
}

/// Output destination accepted by the detached-child path.
pub enum DaemonStdioSource<'a> {
    /// Route output to the platform null device.
    Null,
    /// Duplicate a caller-owned file into the child.
    File(&'a std::fs::File),
}

/// Standard-stream source or destination for a contained child.
pub enum StdioSource<'a> {
    /// Route the stream to the platform null device.
    Null,
    /// Inherit the matching stream from the parent process.
    Parent,
    /// Duplicate a caller-owned file into the child.
    File(&'a std::fs::File),
    /// Create and return an anonymous parent/child pipe pair.
    Pipe,
}

/// Handle for a detached child that is not terminated when dropped.
pub struct DaemonChild {
    pub(crate) pid: u32,
    pub(crate) inner: Box<dyn DaemonChildControl>,
}

pub(crate) trait DaemonChildControl:
    Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe
{
    fn kill(&mut self) -> std::io::Result<()>;
    fn wait(&mut self) -> std::io::Result<i32>;
    fn try_wait(&mut self) -> std::io::Result<Option<i32>>;
}

impl DaemonChild {
    /// Return the operating-system process identifier.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Terminate the child process.
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.inner.kill()
    }

    /// Wait for the child and return its numeric exit code.
    pub fn wait(&mut self) -> std::io::Result<i32> {
        self.inner.wait()
    }

    /// Return the exit code if the child has finished without blocking.
    pub fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        self.inner.try_wait()
    }
}

/// Handle and optional parent pipe ends for a contained child.
///
/// Dropping this value shuts down the contained process group.
pub struct SpawnedChild {
    /// Writable parent end when standard input was configured as a pipe.
    pub stdin: Option<std::process::ChildStdin>,
    /// Readable parent end when standard output was configured as a pipe.
    pub stdout: Option<std::process::ChildStdout>,
    /// Readable parent end when standard error was configured as a pipe.
    pub stderr: Option<std::process::ChildStderr>,
    pub(crate) pid: u32,
    pub(crate) inner: Box<dyn SpawnedChildControl>,
}

pub(crate) trait SpawnedChildControl:
    Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe
{
    fn kill(&mut self) -> std::io::Result<()>;
    fn wait(&mut self) -> std::io::Result<i32>;
    fn try_wait(&mut self) -> std::io::Result<Option<i32>>;
    fn shutdown(&mut self);
}

impl SpawnedChild {
    /// Return the operating-system process identifier.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Forcibly terminate the child on a best-effort basis.
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.inner.kill()
    }

    /// Wait for the child and return its numeric exit code.
    pub fn wait(&mut self) -> std::io::Result<i32> {
        self.inner.wait()
    }

    /// Return the exit code if the child has finished without blocking.
    pub fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        self.inner.try_wait()
    }
}

impl Drop for SpawnedChild {
    fn drop(&mut self) {
        self.inner.shutdown();
    }
}

#[derive(Clone, Copy)]
pub enum ObserverScope {
    SystemWide,
    LaunchedProcessTree,
}
#[derive(Clone, Copy)]
pub enum ObserverCategory {
    File,
    Network,
    Process,
}
#[derive(Clone, Copy)]
pub enum ObserverSupport {
    Supported,
    Partial,
    Unavailable,
}
#[derive(Clone, Copy)]
pub struct ObserverBackend {
    pub support: ObserverSupport,
    pub backend: &'static str,
    pub reason: &'static str,
}
pub use crate::platform_imp::observer_backend;
pub use crate::platform_imp::read_process_cmdline;
pub use crate::platform_imp::read_process_file_handles;

/// Platform-neutral Unix signal selectors used by the compatibility facade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixSignalKind {
    Interrupt,
    Terminate,
    Kill,
}

pub use crate::{
    unix_set_priority, unix_signal_process, unix_signal_process_group, unix_signal_raw,
};

/// What this host installed so a child outlives its owner no longer than it
/// should.
///
/// The variants name the *guarantee*, not the call that produced it. A caller
/// deciding whether to spawn a supervisor cares that the kernel will not do
/// the reaping for it; whether the kernel would have used a parent-death
/// signal or a job object is not a distinction it can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerDeathCleanup {
    /// The kernel signals this process when its owner exits.
    OwnerDeathSignal,
    /// This process belongs to a container the kernel destroys with its owner.
    KillOnOwnerHandleClose,
    /// This process was already in such a container, installed by someone else.
    AlreadyContained,
    /// The host offers no kernel mechanism; a supervisor must do the reaping.
    SupervisorRequired,
    /// The host offers nothing and no supervisor contract is defined here.
    Unsupported,
}

/// Which step of installing owner-death containment failed.
///
/// The caller's operator-facing messages distinguish these, and rightly: not
/// being allowed to *build* a container is a different situation from
/// building one and not being allowed to *join* it. Collapsing both into one
/// error would make the two indistinguishable in a log, so the stage travels
/// with the error rather than being inferred from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerDeathCleanupStage {
    /// Asking the kernel to signal this process when its owner exits.
    RequestSignal,
    /// Creating the container that the kernel destroys with its owner.
    CreateContainer,
    /// Placing this process inside that container.
    JoinContainer,
}

/// A failure to install owner-death containment, and the step it failed at.
#[derive(Debug)]
pub struct OwnerDeathCleanupError {
    /// The step that failed.
    pub stage: OwnerDeathCleanupStage,
    /// What the host reported.
    pub source: std::io::Error,
}

impl std::fmt::Display for OwnerDeathCleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.stage, self.source)
    }
}

impl std::error::Error for OwnerDeathCleanupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub use crate::{
    process_install_owner_death_cleanup as install_owner_death_cleanup,
    process_owner_death_cleanup_target as owner_death_cleanup_target,
};

/// Why a host could not answer a question about a process.
///
/// The three named cases are the ones a caller can act on: a PID that could
/// never name a process, a process that is not there, and a question this
/// host does not answer. Everything else is the host's own report, kept
/// whole rather than flattened into one of the three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessInspectErrorKind {
    /// The PID is outside the range this host issues.
    InvalidPid,
    /// No process on this host currently has that PID.
    NotFound,
    /// This host has no such primitive.
    Unsupported,
    /// The host was asked and refused, or failed.
    Host,
}

/// A failure to inspect or signal a process, and what kind of failure it was.
#[derive(Debug)]
pub struct ProcessInspectError {
    /// Which of the four situations this is.
    pub kind: ProcessInspectErrorKind,
    /// What the host reported.
    pub source: std::io::Error,
}

impl ProcessInspectError {
    /// Build an error of `kind` carrying the host's last reported error.
    pub fn last_os_error(kind: ProcessInspectErrorKind) -> Self {
        Self {
            kind,
            source: std::io::Error::last_os_error(),
        }
    }

    /// Build an error of `kind` with a message this crate composed itself.
    pub fn stated(kind: ProcessInspectErrorKind, message: &str) -> Self {
        Self {
            kind,
            source: std::io::Error::other(message.to_string()),
        }
    }
}

impl std::fmt::Display for ProcessInspectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.source)
    }
}

impl std::error::Error for ProcessInspectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub use crate::{
    process_executable_path as executable_path, process_force_kill as force_kill,
    process_same_executable_path as same_executable_path,
    process_signal_terminate as signal_terminate, ProcessLiveness,
};

/// A standing request from the host that this process shut down.
///
/// Hosts deliver this differently -- a POSIX signal, a Windows console
/// control event injected on a thread of the OS's choosing -- but both arrive
/// in a context where almost nothing is safe to do. A handler may not
/// allocate, log, take a lock, or join a thread. So neither host runs the
/// caller's code: each sets one flag, and the caller reads it whenever it is
/// somewhere it can act.
///
/// That is why this is a poll rather than a callback. A callback would invite
/// exactly the work the delivery context forbids.
pub struct ShutdownRequest {
    flag: &'static std::sync::atomic::AtomicBool,
}

impl ShutdownRequest {
    /// Build a handle watching a flag the caller already owns.
    ///
    /// The host implementations use this to hand out a view of their own
    /// static. It is public because a caller that already has a shutdown flag
    /// -- one set by a supervisor protocol, or by a test -- can present it
    /// through the same type rather than the loop it feeds needing two shapes
    /// of "should I stop".
    ///
    /// `'static` is not incidental: a handler set by the OS outlives any
    /// scope, so the flag it writes has to as well.
    pub fn watching(flag: &'static std::sync::atomic::AtomicBool) -> Self {
        Self { flag }
    }

    /// Whether the host has asked this process to shut down.
    ///
    /// Latching, not edge-triggered: once true it stays true, so a caller that
    /// checks between two pieces of work cannot miss a request delivered while
    /// it was busy.
    pub fn requested(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl std::fmt::Debug for ShutdownRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShutdownRequest")
            .field("requested", &self.requested())
            .finish()
    }
}

pub use crate::process_install_shutdown_request_handler as install_shutdown_request_handler;

/// Whether this host can replace the running image with another program.
///
/// Unix can: `execve` keeps the process -- its PID, its open descriptors,
/// its place in the process tree -- and swaps the program underneath.
/// Windows has no equivalent; the nearest thing is starting a successor and
/// exiting, which is a *different* process with a different PID and does not
/// keep anything a parent or supervisor was holding onto.
///
/// Callers that can accept a successor should ask this and fall back. Callers
/// that genuinely need the same process to continue have no fallback, and
/// should treat `false` as unsupported rather than approximating it.
pub use crate::{
    process_can_replace_current_image as can_replace_current_image,
    process_replace_current_image as replace_current_image,
};
