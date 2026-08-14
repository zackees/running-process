//! Process spawning, containment, inspection, termination, and stdio.

pub use crate::{
    PlatformChild, PlatformEmergencySignal, PlatformLifecycle, PlatformOutput, PlatformStdin,
    SpawnSpec, StreamMode,
};

pub use crate::platform_imp::{
    configure_sync_contained_command, configure_sync_daemon_command, configure_trampoline_command,
    enable_descendant_subreaper, exit_code, parent_has_console, process_snapshot,
    process_snapshot_for_pid, set_process_name, spawn_sync, spawn_sync_daemon,
    sync_child_native_handle, trampoline_exit_code, unix_mark_extra_fds_close_on_exec,
};

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

pub use crate::platform_imp::{
    unix_set_priority, unix_signal_process, unix_signal_process_group, unix_signal_raw,
};

pub use crate::platform_imp::kill_tree;
