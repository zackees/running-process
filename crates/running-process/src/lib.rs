//! Cross-platform process execution, process-tree control, PTY handling, and
//! broker integration primitives.
//!
//! The crate exposes a synchronous process API through [`NativeProcess`], a
//! contained process-group helper through [`ContainedProcessGroup`], low-level
//! spawn helpers through [`spawn()`] and [`spawn_daemon`], and optional
//! daemon/broker modules behind feature flags.

use std::collections::VecDeque;
use std::io::Read;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::observer::{ObserverEmitter, ProcessWatchEmitter};

pub(crate) use running_process_platform_internal::platform;

#[cfg(feature = "async-process")]
mod async_process;
#[cfg(feature = "async-process")]
mod blocking_island;
#[cfg(feature = "async-process")]
pub use blocking_island::dispatch_blocking as blocking_island_dispatch;
pub mod console_detect;
pub mod containment;
mod descendant_monitor;
pub mod env_vars;
pub mod environment;
mod helpers;
#[cfg(feature = "async-process")]
mod process_runtime;
pub mod window_icon;
// Phase 1 of #221: process-observation capability model + portable
// lifecycle baseline. Core-feature-clean (std-only: mpsc + SystemTime),
// so the started/exited baseline is available to the base library
// without pulling in the daemon runtime.
/// Frozen v1 daemon manifest and service-definition registration substrate.
///
/// This direct persistence surface owns only the v1 registration records,
/// SHA-256 seal/verify rules, host stamp, validated paths, and private-file
/// behavior. It deliberately does not select an IPC endpoint, broker client,
/// daemon runtime, identity probe, or async runtime.
#[cfg(feature = "daemon-registration")]
pub mod daemon_registration;
/// Frozen v2 service-definition registration writer substrate.
///
/// This direct persistence surface owns the established `.servicedef.v2`
/// layout, generated service definition, validation, and owner-private file
/// behavior. It deliberately does not select v2 manifests, a loader, broker
/// negotiation, endpoint transport, identity, or an async runtime.
#[cfg(feature = "daemon-registration-v2")]
pub mod daemon_registration_v2;
// The two registration writer features share only the small path, name, error,
// and owner-private-directory substrate. Keeping it separate from either
// public module prevents v2 persistence from selecting v1's SHA-256 manifest
// support, while retaining exact v1 type identity through re-exports.
#[cfg(any(feature = "daemon-registration", feature = "daemon-registration-v2"))]
pub(crate) mod daemon_registration_common;
/// Frozen v1 `Frame` envelope codec and consumer-protocol registry.
///
/// This direct, transport-free surface is available without broker IPC,
/// daemon identity, hashing, or an async runtime. Broad broker paths
/// re-export these exact items for compatibility.
#[cfg(feature = "frame-v1-codec")]
pub mod frame_v1;
// Host facts are shared by the direct identity probe and persisted v1
// registration. The implementation is deliberately private; registration
// exposes its stable public host-identity path from `daemon_registration`.
#[cfg(any(feature = "backend-identity", feature = "daemon-registration"))]
#[path = "broker/host_identity.rs"]
pub(crate) mod daemon_host_identity;
pub mod observer;
#[cfg(feature = "originator-scan")]
pub mod originator;
pub mod output_log;
// The IPC client owns the generated protocol dependency.  Keeping code
// generation in that optional package means process-only consumers do not
// compile broker schemas or their build dependencies (#1144).
#[cfg(feature = "client")]
/// Prost-generated daemon protocol types used by the client transport.
pub mod proto {
    /// Generated Rust bindings for the `running_process.daemon.v1` protobuf package.
    pub use running_process_protocol::daemon;
}

#[cfg(feature = "client")]
pub mod client;

// Phase 0 of #228: v1 broker module — prost-generated wire types from
// `proto/broker_v1_*.proto`. The broad broker remains a `client` API. The
// narrow identity substrate compiles this module privately so its direct
// facade can preserve the frozen v1 probe/frame bytes without exposing broker
// ownership, configuration, or client APIs.
#[cfg(feature = "client")]
pub mod broker;
// The direct facade imports a deliberately small subset of the legacy
// namespace while its compatibility re-exports remain available for type
// identity.  The remaining client-only paths are intentionally dormant here.
#[cfg(all(feature = "backend-identity", not(feature = "client")))]
#[allow(dead_code, unused_imports)]
mod broker;

/// Direct daemon-identity substrate for an existing endpoint.
///
/// This is intentionally a small facade over the frozen v1 identity probe,
/// sidecar, and sans-I/O endpoint mux. It does not adopt the broker client or
/// daemon runtime, and it leaves endpoint naming and application payloads to
/// the caller.
#[cfg(feature = "backend-identity")]
pub mod backend_identity;

// #891: content-hash primitive (`blake3_file`) for dev daemon-identity
// isolation. The direct identity facade needs it internally, but it remains a
// public client-only utility rather than widening the direct facade.
#[cfg(feature = "client")]
pub mod content_hash;
#[cfg(all(feature = "backend-identity", not(feature = "client")))]
mod content_hash;

/// Probe client facade (#633). Gated on the `probe` feature so a build
/// without it contains none of this code.
#[cfg(feature = "probe")]
pub mod probe;

// Phase 1 of #228 (issue #230): maintenance subcommands exposed via
// the `runpm` CLI. Currently just `release-handles` — a cross-platform
// scaffold for the Windows worktree-teardown handle-race fix
// (soldr#710). Gated behind `feature = "client"` because the CLI that
// drives it is.
#[cfg(feature = "client")]
pub mod maintenance;

#[cfg(feature = "client")]
pub mod cleanup;

// Phase 4 of #222 (#427): per-OS boot autostart for the runpm daemon.
// Gated behind `feature = "client"` because the only consumer is the
// `runpm` CLI binary, which is itself client-gated.
#[cfg(feature = "client")]
pub mod boot_autostart;

// Phase 5 of #222 (#428): `runpm.toml` parser used by the `runpm` CLI
// to batch-start `[[app]]` entries. Lives in the library (not under
// `src/bin/`) so the integration test in `tests/runpm/runpm_toml_config.rs`
// can drive the same code path the binary uses.
#[cfg(feature = "client")]
pub mod runpm_config;

// #415: consumer-consumable conformance test kit. Gated behind the
// off-by-default `test-support` cargo feature (which implies `client`)
// so the helpers ship in the published crate but only compile when a
// consumer opts in as a dev-dependency.
#[cfg(feature = "test-support")]
pub mod test_support;

// Lightweight tee sink primitives for callers that want transcript/log
// fan-out without pulling in the full daemon runtime.
//
// The file lives under `daemon/` because that is who else uses it, and the
// `daemon` feature loads it there as `daemon::telemetry`. Declaring it as a
// module here too would load one file as two modules -- two copies of every
// type, which are then not the same type -- so when both features are on this
// re-exports the daemon's module instead of declaring a second one.
#[cfg(all(feature = "telemetry", not(feature = "daemon")))]
#[path = "daemon/telemetry.rs"]
pub mod telemetry;

#[cfg(all(feature = "telemetry", feature = "daemon"))]
pub use daemon::telemetry;

/// `telemetry` and `daemon::telemetry` must name one module, not two copies.
///
/// A `#[path]` module declaration alongside the daemon's own would compile --
/// that was the bug -- but it would mint a second, incompatible set of types
/// from the same source file, so a `TeeHandle` obtained through one path
/// could not be passed to a function expecting the other. This conversion is
/// the identity only while both paths resolve to the same item; if the
/// duplicate declaration ever comes back, it stops compiling here rather than
/// at whichever caller first tried to mix the two.
#[cfg(all(feature = "telemetry", feature = "daemon"))]
const _: fn(crate::telemetry::TeeHandle) -> daemon::telemetry::TeeHandle = |handle| handle;

// Wave 5 of #165: daemon runtime absorbed from `running-process-daemon`.
// Heavy deps (tokio, sqlite, etc.) gated behind `feature = "daemon"`.
#[cfg(feature = "daemon")]
/// Daemon runtime APIs and helpers enabled by the `daemon` feature.
pub mod daemon;
// `kill_tree` is established 4.x containment surface and remains available to
// `default-features = false` callers. Its sysinfo-backed platform primitive is
// the explicit Phase 0.5 compatibility exception; public inspection APIs stay
// behind `process-inspection`.
pub mod process_tree;
#[cfg(feature = "pty")]
/// PTY-backed process APIs.
pub mod pty;
mod public_symbols;
mod rust_debug;
pub mod spawn;
pub mod systemd_killmode;
#[cfg(feature = "terminal-graphics")]
pub mod terminal_graphics;
mod types;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(feature = "async-process")]
pub use async_process::{
    AsyncCapturedOutput, AsyncProcess, AsyncProcessBuilder, AsyncProcessSession,
    AsyncProcessSessionChunk, AsyncProcessSessionControl, AsyncProcessSessionEvent,
    AsyncProcessSessionOptions, AsyncProcessSessionOutput, AsyncStdio,
};
pub use console_detect::{monitor_console_windows, ConsoleWindowInfo};
pub use containment::{ContainedProcessGroup, ORIGINATOR_ENV_VAR};
// #891: content-hash primitive for dev daemon-identity isolation.
#[cfg(feature = "client")]
pub use content_hash::blake3_file;
pub use observer::{
    CapabilitySupport, CaptureSource, CategoryCapability, DumpResult, EventCategory,
    ObservationGrade, ObservationPolicy, ObserverCapabilities, ObserverConfig, ObserverEvent,
    ObserverEventKind, ObserverSubscriber, ProcessEvent, ProcessEventKind, ProcessIdentity,
    ProcessObservation, ProcessObservationCapabilities, ProcessObservationError, ProcessWatch,
    ProcessWatchConfigurationError, ProcessWatchCursor, ProcessWatchGap, ProcessWatchLoss,
    ProcessWatchMatch, ProcessWatchRead, ProcessWatchSubscriber, StackCapture, StackDump,
};
#[cfg(feature = "originator-scan")]
pub use originator::{
    find_declared_daemon_pids, find_processes_by_originator, OriginatorProcessInfo,
};
pub use output_log::{
    CursorRead, OutputCursor, OutputLog, OutputRecord, SharedOutputCursor, SharedOutputLog,
};
/// Executable naming and image-relative discovery, for binaries in this
/// workspace that must name a sibling program without spelling it per host.
#[doc(hidden)]
pub use running_process_platform_internal::platform::executable as platform_executable;
#[cfg(target_os = "linux")]
pub use running_process_platform_internal::platform::process::current_executable_build_id;
pub use rust_debug::{render_rust_debug_traces, RustDebugScopeGuard};
pub use spawn::{
    spawn, spawn_daemon, spawn_daemon_breaking_away_from_job,
    spawn_daemon_breaking_away_with_env_policy, spawn_daemon_with_clear_env,
    spawn_daemon_with_env_policy, spawn_daemon_with_stdio, spawn_daemon_with_stdio_and_env_policy,
    spawn_with_env_policy, DaemonChild, DaemonStdio, DaemonStdioSource, EnvironmentPolicy,
    SpawnStdio, SpawnedChild, StdioSource, DAEMON_MARKER_ENV_VAR,
};
#[cfg(feature = "client-async")]
pub use spawn::{spawn_tokio, TokioSpawnOptions};
#[cfg(feature = "terminal-graphics")]
pub use terminal_graphics::{
    current_terminal_capabilities, current_terminal_capabilities_with_timeout,
    detect_terminal_capabilities, CapabilityStatus, EvidenceStrength, GraphicsCapability,
    GraphicsProtocol, TerminalCapabilities, TerminalCapabilityInput, TerminalGraphicsCapabilities,
    TerminalProbeEvidence,
};
pub use types::{
    CommandSpec, ProcessConfig, ProcessError, ReadStatus, RunOutput, StderrMode, StdinMode,
    StreamEvent, StreamKind,
};
pub use window_icon::{
    host_icon_support, icon_support, set_host_icon, set_icon, IconError, IconScope, IconSource,
    IconSupport, StockIcon,
};

#[cfg(unix)]
pub(crate) use helpers::{child_try_wait_error_is_retryable, poll_mutex_until};
pub(crate) use helpers::{exit_code, feed_chunk, kill_drain_deadline, log_spawned_child_pid};
#[cfg(unix)]
pub use unix::{unix_set_priority, unix_signal_process, unix_signal_process_group, UnixSignal};
#[cfg(windows)]
pub(crate) use windows::{assign_child_to_windows_kill_on_close_job_impl, WindowsJobHandle};

#[macro_export]
/// Create a scoped Rust debug trace label for the current function body.
macro_rules! rp_rust_debug_scope {
    ($label:expr) => {
        let _running_process_rust_debug_scope =
            $crate::RustDebugScopeGuard::enter($label, file!(), line!());
    };
}

#[derive(Default)]
struct QueueState {
    stdout_queue: VecDeque<Vec<u8>>,
    stderr_queue: VecDeque<Vec<u8>>,
    combined_queue: VecDeque<StreamEvent>,
    stdout_history: VecDeque<Vec<u8>>,
    stderr_history: VecDeque<Vec<u8>>,
    combined_history: VecDeque<StreamEvent>,
    stdout_raw: Vec<u8>,
    stderr_raw: Vec<u8>,
    stdout_history_bytes: usize,
    stderr_history_bytes: usize,
    combined_history_bytes: usize,
    stdout_closed: bool,
    stderr_closed: bool,
}

/// Sentinel value for returncode atomic: process has not exited yet.
const RETURNCODE_NOT_SET: i64 = i64::MIN;

struct SharedState {
    queues: Mutex<QueueState>,
    condvar: Condvar,
    capture_limit: Option<usize>,
    capture_overflowed: AtomicBool,
    active_capture_readers: std::sync::atomic::AtomicUsize,
    /// Atomic exit code. `RETURNCODE_NOT_SET` means "not exited yet".
    /// Updated by a background waiter thread — reading is lock-free.
    returncode: AtomicI64,
    /// Phase 1 of #221: optional lifecycle-event emitter. `None` means
    /// observation is off (the off-by-default path), so the lifecycle
    /// hooks are inert. When `Some`, `started` is emitted once at spawn
    /// and `exited` exactly once on the first returncode transition.
    observer: Option<ObserverEmitter>,
    /// Guards against emitting more than one `exited` event when several
    /// code paths (waiter thread, `poll`, `kill`) race to record the exit.
    observer_exit_emitted: AtomicBool,
}

struct ChildState {
    child: ChildHandle,
    #[cfg(windows)]
    _job: WindowsJobHandle,
}

enum ChildHandle {
    Standard(Child),
    ExactTrace(running_process_platform_internal::platform::process::TracedChild),
}

impl ChildHandle {
    fn id(&self) -> u32 {
        match self {
            Self::Standard(child) => child.id(),
            Self::ExactTrace(child) => child.id(),
        }
    }

    fn try_wait_code(&mut self) -> std::io::Result<Option<i32>> {
        match self {
            Self::Standard(child) => child.try_wait().map(|status| status.map(exit_code)),
            Self::ExactTrace(child) => child.try_wait_code(),
        }
    }

    fn kill(&mut self) -> std::io::Result<()> {
        match self {
            Self::Standard(child) => child.kill(),
            Self::ExactTrace(child) => child.kill(),
        }
    }

    fn take_stdin(&mut self) -> Option<ChildStdin> {
        match self {
            Self::Standard(child) => child.stdin.take(),
            Self::ExactTrace(child) => child.take_stdin(),
        }
    }

    fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        match self {
            Self::Standard(child) => child.stdout.take(),
            Self::ExactTrace(child) => child.take_stdout(),
        }
    }

    fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        match self {
            Self::Standard(child) => child.stderr.take(),
            Self::ExactTrace(child) => child.take_stderr(),
        }
    }

    #[cfg(windows)]
    fn wait_code(&mut self) -> std::io::Result<i32> {
        match self {
            Self::Standard(child) => child.wait().map(exit_code),
            Self::ExactTrace(child) => child.wait_code(),
        }
    }
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
enum CapturePollAction {
    Wait,
    Read,
    Cancel,
}

#[cfg(test)]
fn capture_poll_action(capture_revents: i16, wake_revents: i16) -> CapturePollAction {
    if wake_revents != 0 {
        CapturePollAction::Cancel
    } else if capture_revents != 0 {
        CapturePollAction::Read
    } else {
        CapturePollAction::Wait
    }
}

fn cleanup_child_after_start_error(child: ChildHandle) {
    match child {
        ChildHandle::Standard(mut child) => {
            let _ = child.kill();
            // Keep start bounded while retaining ownership until the child is
            // eventually reaped, even if SIGKILL delivery takes time.
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
        ChildHandle::ExactTrace(mut child) => {
            // The dedicated tracer remains the sole waiter and will reap it.
            let _ = child.kill();
        }
    }
}

impl SharedState {
    #[cfg(test)]
    fn new(capture: bool) -> Self {
        Self::with_observer_and_limit(capture, None, None)
    }

    fn with_observer_and_limit(
        capture: bool,
        observer: Option<ObserverEmitter>,
        capture_limit: Option<usize>,
    ) -> Self {
        let queues = QueueState {
            stdout_closed: !capture,
            stderr_closed: !capture,
            ..QueueState::default()
        };
        Self {
            queues: Mutex::new(queues),
            condvar: Condvar::new(),
            capture_limit,
            capture_overflowed: AtomicBool::new(false),
            active_capture_readers: std::sync::atomic::AtomicUsize::new(0),
            returncode: AtomicI64::new(RETURNCODE_NOT_SET),
            observer,
            observer_exit_emitted: AtomicBool::new(false),
        }
    }

    /// Emit the lifecycle `exited` event exactly once, regardless of which
    /// code path first observes the exit. No-op when observation is off.
    fn emit_exited(&self, pid: u32, exit_code: i32) {
        let Some(emitter) = self.observer.as_ref() else {
            return;
        };
        if self
            .observer_exit_emitted
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            emitter.emit_exited(pid, exit_code);
        }
    }
}

/// A cross-platform child process with optional output capture.
///
/// `NativeProcess` wraps [`std::process::Command`] with the crate's
/// process-tree containment, capture draining, timeout, and terminal-control
/// behavior. Methods are synchronous and are safe to call from ordinary
/// blocking code.
pub struct NativeProcess {
    config: ProcessConfig,
    command_override: Mutex<Option<Command>>,
    child: Arc<Mutex<Option<ChildState>>>,
    stdin: Mutex<Option<ChildStdin>>,
    shared: Arc<SharedState>,
    process_watch: Option<Arc<ProcessWatchEmitter>>,
    // This remains a constructor-only policy for the bounded std::Command
    // entrypoint. General NativeProcess callers keep their established
    // platform policy surface.
    kill_when_owner_dies: bool,
    #[cfg(test)]
    stdin_write_active: AtomicBool,
    capture_cancellation:
        Arc<running_process_platform_internal::platform::process::CaptureCancellation>,
}

impl NativeProcess {
    /// Create a process wrapper from a [`ProcessConfig`].
    ///
    /// The child is not spawned until [`Self::start`] is called. Process
    /// observation is **off by default**: no lifecycle events are emitted
    /// unless [`Self::with_observer`] is used instead.
    pub fn new(config: ProcessConfig) -> Self {
        Self::new_with_options(config, None, None, None, None)
    }

    /// Create a process wrapper with process observation enabled (Phase 1
    /// of #221).
    ///
    /// Returns the wrapper paired with an [`ObserverSubscriber`] that
    /// receives a [`started`](crate::ObserverEventKind::Started) event when
    /// [`Self::start`] spawns the child and exactly one
    /// [`exited`](crate::ObserverEventKind::Exited) event when the child is
    /// reaped — for the categories the `config` requests that are actually
    /// `Supported` (only [`Lifecycle`](crate::EventCategory::Lifecycle) in
    /// Phase 1; see [`ObserverCapabilities::negotiate`](crate::ObserverCapabilities::negotiate)).
    ///
    /// The emitter never blocks on a slow or dropped subscriber.
    pub fn with_observer(
        config: ProcessConfig,
        observer: crate::observer::ObserverConfig,
    ) -> (Self, ObserverSubscriber) {
        let (emitter, subscriber) = ObserverEmitter::new(observer);
        let process = Self::new_with_options(config, Some(emitter), None, None, None);
        (process, subscriber)
    }

    /// Create a process wrapper from a caller-configured
    /// [`std::process::Command`] with process observation enabled.
    ///
    /// [`ProcessConfig`]'s declarative surface deliberately cannot represent
    /// everything a `Command` can carry — `env_remove` scrubs of inherited
    /// variables, non-Unicode (`OsString`) argv/env values, a pre-resolved
    /// working directory — so a caller that already owns a fully configured
    /// `Command` (a compiler front door wrapping cargo, zackees/soldr#2546)
    /// would otherwise have to lossily re-encode it. This pairs the observer
    /// machinery with the same command-override seam the capture-limit
    /// constructors use: `command` is spawned verbatim, while `config` still
    /// governs stdio routing, capture, containment, and limits (its
    /// `command` / `cwd` / `env` fields are ignored in favor of the
    /// override, matching `build_command`).
    pub fn with_observer_and_command(
        command: Command,
        config: ProcessConfig,
        observer: crate::observer::ObserverConfig,
    ) -> (Self, ObserverSubscriber) {
        let (emitter, subscriber) = ObserverEmitter::new(observer);
        let process = Self::new_with_options(config, Some(emitter), None, Some(command), None);
        (process, subscriber)
    }

    /// Create a process with launched-tree watch matching configured before
    /// spawn. Exact tracing, when selected, owns the launch-time wait events.
    pub fn with_process_watches(
        config: ProcessConfig,
        watches: Vec<ProcessWatch>,
        policy: ObservationPolicy,
    ) -> Result<(Self, ProcessWatchSubscriber), ProcessObservationError> {
        let (emitter, subscriber) = ProcessWatchEmitter::new(watches, policy)?;
        let process = Self::new_with_options(config, None, None, None, Some(emitter));
        Ok((process, subscriber))
    }

    /// Describe exact launched-tree observation support on this host.
    pub fn process_observation_capabilities() -> ProcessObservationCapabilities {
        ProcessObservationCapabilities::current()
    }

    fn new_with_capture_limit(config: ProcessConfig, capture_limit: usize) -> Self {
        Self::new_with_options(config, None, Some(capture_limit), None, None)
    }

    fn new_with_command_capture_limit(
        command: Command,
        config: ProcessConfig,
        capture_limit: usize,
        kill_when_owner_dies: bool,
    ) -> Self {
        let mut process =
            Self::new_with_options(config, None, Some(capture_limit), Some(command), None);
        process.kill_when_owner_dies = kill_when_owner_dies;
        process
    }

    fn new_with_options(
        config: ProcessConfig,
        observer: Option<ObserverEmitter>,
        capture_limit: Option<usize>,
        command_override: Option<Command>,
        process_watch: Option<Arc<ProcessWatchEmitter>>,
    ) -> Self {
        let shared = SharedState::with_observer_and_limit(config.capture, observer, capture_limit);
        Self {
            shared: Arc::new(shared),
            process_watch,
            command_override: Mutex::new(command_override),
            child: Arc::new(Mutex::new(None)),
            stdin: Mutex::new(None),
            kill_when_owner_dies: false,
            #[cfg(test)]
            stdin_write_active: AtomicBool::new(false),
            config,
            capture_cancellation: Arc::new(Default::default()),
        }
    }

    // Preserve a stable Rust frame here in release user dumps.
    #[inline(never)]
    /// Spawn the configured child process.
    ///
    /// Returns [`ProcessError::AlreadyStarted`] if the same wrapper already
    /// owns a running child.
    pub fn start(&self) -> Result<(), ProcessError> {
        public_symbols::rp_native_process_start_public(self)
    }

    fn start_impl(&self) -> Result<(), ProcessError> {
        crate::rp_rust_debug_scope!("running_process::NativeProcess::start");
        let mut guard = self.child.lock().expect("child mutex poisoned");
        if guard.is_some() {
            return Err(ProcessError::AlreadyStarted);
        }

        let mut command = self.build_command();
        let exact_trace = self
            .process_watch
            .as_ref()
            .is_some_and(|watch| watch.uses_exact_trace());
        match self.config.stdin_mode {
            StdinMode::Inherit => {}
            StdinMode::Piped => {
                command.stdin(Stdio::piped());
            }
            StdinMode::Null => {
                command.stdin(Stdio::null());
            }
        }
        if self.config.capture {
            command.stdout(Stdio::piped());
            command.stderr(Stdio::piped());
        }

        let mut child = if exact_trace {
            let event_watch = Arc::clone(self.process_watch.as_ref().expect("exact watch checked"));
            let completion_watch = Arc::clone(&event_watch);
            match running_process_platform_internal::platform::process::start_exact_trace(
                command,
                Box::new(move |event| event_watch.emit_exact(event)),
                Box::new(move || completion_watch.close()),
            ) {
                Ok(child) => ChildHandle::ExactTrace(child),
                Err(error) => {
                    if let Some(watch) = self.process_watch.as_ref() {
                        watch.close();
                    }
                    return Err(ProcessError::Spawn(error));
                }
            }
        } else {
            ChildHandle::Standard(command.spawn().map_err(ProcessError::Spawn)?)
        };
        log_spawned_child_pid(child.id()).map_err(ProcessError::Spawn)?;
        // Phase 1 of #221: emit the lifecycle `started` event. No-op when
        // observation is off (the common, off-by-default path).
        if let Some(emitter) = self.shared.observer.as_ref() {
            emitter.emit_started(child.id());
        }
        // #539 slice 2: when the observer requests EventCategory::Process,
        // associate an IOCP with the per-spawn Job Object so a pump thread
        // can forward descendant lifecycle events. The Lifecycle category
        // is still served by emit_started / emit_exited above and below.
        #[cfg(windows)]
        let job = {
            let descendant_sink = self
                .shared
                .observer
                .as_ref()
                .and_then(|e| e.descendant_sink());
            let job_result = match &child {
                ChildHandle::Standard(standard_child) => {
                    let direct_pid = standard_child.id();
                    public_symbols::rp_assign_child_to_windows_kill_on_close_job_with_observer_public(
                        standard_child,
                        descendant_sink,
                        self.process_watch.clone(),
                        direct_pid,
                        self.config.address_space_limit_bytes,
                    )
                }
                ChildHandle::ExactTrace(_) => unreachable!("Windows exact tracing is unavailable"),
            };
            match job_result {
                Ok(job) => job,
                Err(error) => {
                    if let Some(watch) = self.process_watch.as_ref() {
                        watch.close();
                    }
                    cleanup_child_after_start_error(child);
                    return Err(ProcessError::Spawn(error));
                }
            }
        };
        if !exact_trace {
            descendant_monitor::start(
                child.id(),
                self.shared.observer.as_ref(),
                self.process_watch.as_ref(),
            );
        }
        if self.config.capture {
            let stdout = child.take_stdout().expect("stdout pipe missing");
            let stderr = child.take_stderr().expect("stderr pipe missing");
            let stdout =
                match running_process_platform_internal::platform::process::prepare_capture_reader(
                    stdout,
                    &self.capture_cancellation,
                    running_process_platform_internal::platform::process::CaptureStream::Stdout,
                ) {
                    Ok(stdout) => stdout,
                    Err(error) => {
                        cleanup_child_after_start_error(child);
                        return Err(ProcessError::Spawn(error));
                    }
                };
            let stderr =
                match running_process_platform_internal::platform::process::prepare_capture_reader(
                    stderr,
                    &self.capture_cancellation,
                    running_process_platform_internal::platform::process::CaptureStream::Stderr,
                ) {
                    Ok(stderr) => stderr,
                    Err(error) => {
                        running_process_platform_internal::platform::process::capture_reader_done(
                        &self.capture_cancellation,
                        running_process_platform_internal::platform::process::CaptureStream::Stdout,
                    );
                        cleanup_child_after_start_error(child);
                        return Err(ProcessError::Spawn(error));
                    }
                };
            self.spawn_reader(
                stdout,
                StreamKind::Stdout,
                StreamKind::Stdout,
                self.pipe_done_callback(StreamKind::Stdout),
            );
            self.spawn_reader(
                stderr,
                StreamKind::Stderr,
                match self.config.stderr_mode {
                    StderrMode::Stdout => StreamKind::Stdout,
                    StderrMode::Pipe => StreamKind::Stderr,
                },
                self.pipe_done_callback(StreamKind::Stderr),
            );
        }
        *self.stdin.lock().expect("stdin mutex poisoned") = child.take_stdin();
        *guard = Some(ChildState {
            child,
            #[cfg(windows)]
            _job: job,
        });
        drop(guard);
        self.spawn_exit_waiter();
        Ok(())
    }

    /// Background thread that polls for process exit and stores the exit code
    /// atomically. This makes `returncode` auto-update without explicit `poll()`.
    fn spawn_exit_waiter(&self) {
        let child = Arc::clone(&self.child);
        let shared = Arc::clone(&self.shared);
        let capture = self.config.capture;
        let capture_cancellation = Arc::clone(&self.capture_cancellation);
        thread::spawn(move || {
            loop {
                if shared.returncode.load(Ordering::Acquire) != RETURNCODE_NOT_SET {
                    return;
                }
                let exited = {
                    let mut guard = child.lock().expect("child mutex poisoned");
                    if let Some(child_state) = guard.as_mut() {
                        let pid = child_state.child.id();
                        match child_state.child.try_wait_code() {
                            Ok(Some(code)) => {
                                shared.returncode.store(code as i64, Ordering::Release);
                                // Phase 1 of #221: lifecycle `exited`. Emit
                                // before notifying waiters and is guarded so
                                // only the first exit-observer fires.
                                shared.emit_exited(pid, code);
                                shared.condvar.notify_all();
                                true
                            }
                            Ok(None) => false,
                            Err(_error) => {
                                #[cfg(unix)]
                                if child_try_wait_error_is_retryable(&_error) {
                                    false
                                } else {
                                    return;
                                }
                                #[cfg(windows)]
                                return;
                            }
                        }
                    } else {
                        return;
                    }
                };
                if exited {
                    // The direct child has exited. Bound the capture-completion
                    // wait so wait()/close()/read_* on the natural-exit path
                    // cannot wedge forever when a grandchild inherited the pipe
                    // and outlives the child (issue #590, cluster A). Unlike
                    // `kill_impl` we do NOT cancel the reader up front: a
                    // short-lived grandchild may still emit output the caller
                    // expects to capture, so the reader is left to drain
                    // naturally within the grace window. Only if the window
                    // elapses with the pipe still held open do we cancel, to
                    // release the otherwise-leaked reader thread (Windows:
                    // CancelIoEx; Unix: a per-reader wake socket). The child
                    // lock is released before this
                    // potentially-blocking finalize so poll()/kill() are never
                    // held off.
                    if capture {
                        let drained = finalize_capture_completion(&shared, kill_drain_deadline());
                        if !drained {
                            running_process_platform_internal::platform::process::cancel_capture_reader(
                                &capture_cancellation,
                            );
                        }
                    }
                    // Non-invasive watch EOF is owned by the platform
                    // descendant backend: Linux/macOS perform one final
                    // reconciliation and Windows waits for
                    // ACTIVE_PROCESS_ZERO. Closing here would race those
                    // final descendant notifications.
                    return;
                }
                // #199: intentional — capture thread polling for
                // child-exit. `try_wait` is non-blocking by design;
                // we can't block here because the thread also drains
                // pipe state alongside the exit check. 10ms keeps the
                // CPU cost negligible while staying responsive.
                thread::sleep(Duration::from_millis(10));
            }
        });
    }

    /// Write bytes to the child's stdin and then close stdin.
    pub fn write_stdin(&self, data: &[u8]) -> Result<(), ProcessError> {
        if self.child.lock().expect("child mutex poisoned").is_none() {
            return Err(ProcessError::NotRunning);
        }
        let mut guard = self.stdin.lock().expect("stdin mutex poisoned");
        let stdin = guard.as_mut().ok_or(ProcessError::StdinUnavailable)?;
        use std::io::Write;
        #[cfg(test)]
        self.stdin_write_active.store(true, Ordering::Release);
        let write_result = stdin.write_all(data);
        #[cfg(test)]
        self.stdin_write_active.store(false, Ordering::Release);
        write_result.map_err(ProcessError::Io)?;
        stdin.flush().map_err(ProcessError::Io)?;
        drop(guard.take());
        Ok(())
    }

    /// Write to the child's stdin without closing it afterwards, so the
    /// caller can issue additional writes. Used by interactive
    /// pipe-backed sessions (#130 milestone 3) where the daemon keeps
    /// stdin open across multiple client input frames.
    pub fn write_stdin_streaming(&self, data: &[u8]) -> Result<(), ProcessError> {
        if self.child.lock().expect("child mutex poisoned").is_none() {
            return Err(ProcessError::NotRunning);
        }
        let mut guard = self.stdin.lock().expect("stdin mutex poisoned");
        let stdin = guard.as_mut().ok_or(ProcessError::StdinUnavailable)?;
        use std::io::Write;
        #[cfg(test)]
        self.stdin_write_active.store(true, Ordering::Release);
        let write_result = stdin.write_all(data);
        #[cfg(test)]
        self.stdin_write_active.store(false, Ordering::Release);
        write_result.map_err(ProcessError::Io)?;
        stdin.flush().map_err(ProcessError::Io)?;
        Ok(())
    }

    /// Explicitly close the child's stdin (signals EOF to the child).
    /// Idempotent: returns Ok if stdin was already closed.
    pub fn close_stdin(&self) -> Result<(), ProcessError> {
        if self.child.lock().expect("child mutex poisoned").is_none() {
            return Err(ProcessError::NotRunning);
        }
        drop(self.stdin.lock().expect("stdin mutex poisoned").take());
        Ok(())
    }

    /// Check whether the child has exited without blocking.
    ///
    /// Returns `Ok(None)` while the process is still running.
    pub fn poll(&self) -> Result<Option<i32>, ProcessError> {
        // Fast path: check atomic set by background waiter thread.
        if let Some(code) = self.returncode() {
            return Ok(Some(code));
        }
        let mut guard = self.child.lock().expect("child mutex poisoned");
        let Some(child_state) = guard.as_mut() else {
            return Ok(self.returncode());
        };
        let pid = child_state.child.id();
        let child = &mut child_state.child;
        let status = child.try_wait_code().map_err(ProcessError::Io)?;
        if let Some(code) = status {
            self.set_returncode(code);
            self.shared.emit_exited(pid, code);
            return Ok(Some(code));
        }
        Ok(None)
    }

    // Preserve a stable Rust frame here in release user dumps.
    #[inline(never)]
    /// Wait for the child to exit.
    ///
    /// When `timeout` is `Some`, returns [`ProcessError::Timeout`] if the
    /// child does not exit before the duration elapses.
    pub fn wait(&self, timeout: Option<Duration>) -> Result<i32, ProcessError> {
        public_symbols::rp_native_process_wait_public(self, timeout)
    }

    fn wait_impl(&self, timeout: Option<Duration>) -> Result<i32, ProcessError> {
        crate::rp_rust_debug_scope!("running_process::NativeProcess::wait");
        if self.child.lock().expect("child mutex poisoned").is_none() {
            return self.returncode().ok_or(ProcessError::NotRunning);
        }
        // Fast path: already exited.
        if let Some(code) = self.returncode() {
            self.finish_capture_drain();
            return Ok(code);
        }
        let start = Instant::now();
        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");
        loop {
            // Check returncode (set by exit-waiter thread via atomic + condvar).
            let rc = self.shared.returncode.load(Ordering::Acquire);
            if rc != RETURNCODE_NOT_SET {
                drop(guard);
                let code = rc as i32;
                self.finish_capture_drain();
                return Ok(code);
            }
            if let Some(limit) = timeout {
                let elapsed = start.elapsed();
                if elapsed >= limit {
                    return Err(ProcessError::Timeout);
                }
                let remaining = limit - elapsed;
                // Wait on condvar with timeout, capped at 50ms to recheck.
                let wait_time = remaining.min(Duration::from_millis(50));
                guard = self
                    .shared
                    .condvar
                    .wait_timeout(guard, wait_time)
                    .expect("queue mutex poisoned")
                    .0;
            } else {
                // Wait on condvar with periodic recheck.
                guard = self
                    .shared
                    .condvar
                    .wait_timeout(guard, Duration::from_millis(50))
                    .expect("queue mutex poisoned")
                    .0;
            }
        }
    }

    // Preserve a stable Rust frame here in release user dumps.
    #[inline(never)]
    /// Forcefully terminate the child process.
    pub fn kill(&self) -> Result<(), ProcessError> {
        public_symbols::rp_native_process_kill_public(self)
    }

    fn kill_impl(&self) -> Result<(), ProcessError> {
        crate::rp_rust_debug_scope!("running_process::NativeProcess::kill");
        #[cfg(windows)]
        {
            let mut guard = self.child.lock().expect("child mutex poisoned");
            let child = &mut guard.as_mut().ok_or(ProcessError::NotRunning)?.child;
            let pid = child.id();
            child.kill().map_err(ProcessError::Io)?;
            let code = child.wait_code().map_err(ProcessError::Io)?;
            self.set_returncode(code);
            // Phase 1 of #221: a killed child still produces a lifecycle
            // `exited` event (guarded against double-emit by the waiter).
            self.shared.emit_exited(pid, code);
        }
        #[cfg(unix)]
        {
            let deadline = kill_drain_deadline();
            let (pid, already_reaped) = {
                let mut state = self.child.lock().expect("child mutex poisoned");
                let child = &mut state.as_mut().ok_or(ProcessError::NotRunning)?.child;
                let pid = child.id();
                if let Some(code) = child.try_wait_code().map_err(ProcessError::Io)? {
                    (pid, Some(code))
                } else {
                    let group_signaled = self.config.create_process_group
                        && unix_signal_process_group(pid as i32, UnixSignal::Kill).is_ok();
                    if !group_signaled {
                        child.kill().map_err(ProcessError::Io)?;
                    }
                    (pid, None)
                }
            };

            // Wake capture readers immediately after signal delivery. In
            // particular, this prevents a surviving pipe-owning descendant
            // from extending the bounded reap window.
            self.cancel_capture_io();
            let reaped = already_reaped.or_else(|| {
                let reap_result =
                    poll_mutex_until(&self.child, deadline, Duration::from_millis(10), |state| {
                        match state.as_mut() {
                            Some(child) => child.child.try_wait_code(),
                            None => Ok(None),
                        }
                    });
                match reap_result {
                    Ok(Some(code)) => Some(code),
                    _ => None,
                }
            });
            if let Some(code) = reaped {
                self.set_returncode(code);
                self.shared.emit_exited(pid, code);
            }
            public_symbols::rp_native_process_wait_for_capture_completion_with_deadline_public(
                self, deadline,
            );
            Ok(())
        }
        #[cfg(windows)]
        {
            // Interrupt any pending capture `read()` in the per-stream reader
            // threads so they fall out of their loops immediately. This is what
            // makes the grandchild-pipe-orphan
            // case (FastLED Bug B: uv.exe spawns a python.exe grandchild
            // that inherits the pipe and outlives uv) wake up in
            // microseconds instead of waiting for the bounded-drain
            // safety-net deadline below.
            self.cancel_capture_io();
            // Synchronize with the per-stream reader threads so that by the
            // time kill() returns, the capture queues have flipped from
            // "blocked on read" to "closed" and downstream pollers (e.g.
            // take_combined_line) observe EOS instead of timeout. Without
            // this, callers that hit a wait()-timeout path see Python code
            // raise TimeoutError, kill the child, then race the reader
            // threads — a 10ms poll loop can miss the EOS flip entirely.
            //
            // The deadline remains a safety-net if the platform wake mechanism
            // does not fire.
            public_symbols::rp_native_process_wait_for_capture_completion_with_deadline_public(
                self,
                kill_drain_deadline(),
            );
            Ok(())
        }
    }

    /// Terminate the child process.
    ///
    /// This currently uses the same hard-kill path as [`Self::kill`].
    pub fn terminate(&self) -> Result<(), ProcessError> {
        self.kill()
    }

    /// Send the OS-appropriate soft termination signal to the child's
    /// process group (POSIX: SIGTERM to `-pid`; Windows: Ctrl+Break).
    ///
    /// Requires `ProcessConfig.create_process_group=true` on POSIX so
    /// that `-pid` resolves to the child's own group. With the default
    /// `create_process_group=false`, the kill would walk back to the
    /// caller's group; the method silently no-ops in that case to avoid
    /// signaling the wrong tree.
    ///
    /// Used by the daemon-side pipe sessions (#130 M4 follow-up) so
    /// that `TerminationOutcome::SoftExit` becomes meaningful on POSIX.
    pub fn terminate_group_soft(&self) -> Result<(), ProcessError> {
        if !self.config.create_process_group {
            // A group signal would otherwise reach the caller's own group.
            return Ok(());
        }
        let pid = self.pid().ok_or(ProcessError::NotRunning)?;
        running_process_platform_internal::platform::process::soft_terminate_process_group(pid)
            .map_err(ProcessError::Io)
    }

    // Preserve a stable Rust frame here in release user dumps.
    #[inline(never)]
    /// Close the process wrapper by terminating the child when it is running.
    pub fn close(&self) -> Result<(), ProcessError> {
        public_symbols::rp_native_process_close_public(self)
    }

    fn close_impl(&self) -> Result<(), ProcessError> {
        crate::rp_rust_debug_scope!("running_process::NativeProcess::close");
        if self.child.lock().expect("child mutex poisoned").is_none() {
            return Ok(());
        }
        if self.poll()?.is_none() {
            self.kill()?;
        } else {
            self.finish_capture_drain();
        }
        if let Some(watch) = self.process_watch.as_ref() {
            watch.close();
        }
        Ok(())
    }

    /// Return the child process id when the wrapper currently owns a child.
    pub fn pid(&self) -> Option<u32> {
        self.child
            .lock()
            .expect("child mutex poisoned")
            .as_ref()
            .map(|state| state.child.id())
    }

    /// Return the cached exit code when the child has exited.
    pub fn returncode(&self) -> Option<i32> {
        let v = self.shared.returncode.load(Ordering::Acquire);
        if v == RETURNCODE_NOT_SET {
            None
        } else {
            Some(v as i32)
        }
    }

    /// Return whether captured output is queued for one stream.
    pub fn has_pending_stream(&self, stream: StreamKind) -> bool {
        if stream == StreamKind::Stderr && self.config.stderr_mode == StderrMode::Stdout {
            return false;
        }
        let guard = self.shared.queues.lock().expect("queue mutex poisoned");
        match stream {
            StreamKind::Stdout => !guard.stdout_queue.is_empty(),
            StreamKind::Stderr => !guard.stderr_queue.is_empty(),
        }
    }

    /// Return whether captured combined output is queued.
    pub fn has_pending_combined(&self) -> bool {
        let guard = self.shared.queues.lock().expect("queue mutex poisoned");
        !guard.combined_queue.is_empty()
    }

    /// Drain and return all queued output for one stream.
    pub fn drain_stream(&self, stream: StreamKind) -> Vec<Vec<u8>> {
        if stream == StreamKind::Stderr && self.config.stderr_mode == StderrMode::Stdout {
            return Vec::new();
        }
        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");
        let queue = match stream {
            StreamKind::Stdout => &mut guard.stdout_queue,
            StreamKind::Stderr => &mut guard.stderr_queue,
        };
        queue.drain(..).collect()
    }

    /// Drain and return all queued combined output events.
    pub fn drain_combined(&self) -> Vec<StreamEvent> {
        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");
        guard.combined_queue.drain(..).collect()
    }

    /// Read the next captured chunk from one stream.
    ///
    /// Returns [`ReadStatus::Timeout`] when `timeout` elapses before output or
    /// EOF is observed.
    pub fn read_stream(
        &self,
        stream: StreamKind,
        timeout: Option<Duration>,
    ) -> ReadStatus<Vec<u8>> {
        let deadline = timeout.map(|limit| Instant::now() + limit);
        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");

        loop {
            if stream == StreamKind::Stderr && self.config.stderr_mode == StderrMode::Stdout {
                return ReadStatus::Eof;
            }

            let queue = match stream {
                StreamKind::Stdout => &mut guard.stdout_queue,
                StreamKind::Stderr => &mut guard.stderr_queue,
            };
            if let Some(line) = queue.pop_front() {
                return ReadStatus::Line(line);
            }

            let closed = match stream {
                StreamKind::Stdout => {
                    if self.config.stderr_mode == StderrMode::Stdout {
                        guard.stdout_closed && guard.stderr_closed
                    } else {
                        guard.stdout_closed
                    }
                }
                StreamKind::Stderr => guard.stderr_closed,
            };
            if closed {
                return ReadStatus::Eof;
            }

            match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return ReadStatus::Timeout;
                    }
                    let wait = deadline.saturating_duration_since(now);
                    let result = self
                        .shared
                        .condvar
                        .wait_timeout(guard, wait)
                        .expect("queue mutex poisoned");
                    guard = result.0;
                    if result.1.timed_out() {
                        return ReadStatus::Timeout;
                    }
                }
                None => {
                    guard = self
                        .shared
                        .condvar
                        .wait(guard)
                        .expect("queue mutex poisoned");
                }
            }
        }
    }

    // Preserve a stable Rust frame here in release user dumps.
    #[inline(never)]
    /// Read the next captured combined stream event.
    pub fn read_combined(&self, timeout: Option<Duration>) -> ReadStatus<StreamEvent> {
        public_symbols::rp_native_process_read_combined_public(self, timeout)
    }

    fn read_combined_impl(&self, timeout: Option<Duration>) -> ReadStatus<StreamEvent> {
        crate::rp_rust_debug_scope!("running_process::NativeProcess::read_combined");
        let deadline = timeout.map(|limit| Instant::now() + limit);
        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");

        loop {
            if let Some(event) = guard.combined_queue.pop_front() {
                return ReadStatus::Line(event);
            }
            if guard.stdout_closed && guard.stderr_closed {
                return ReadStatus::Eof;
            }

            match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return ReadStatus::Timeout;
                    }
                    let wait = deadline.saturating_duration_since(now);
                    let result = self
                        .shared
                        .condvar
                        .wait_timeout(guard, wait)
                        .expect("queue mutex poisoned");
                    guard = result.0;
                    if result.1.timed_out() {
                        return ReadStatus::Timeout;
                    }
                }
                None => {
                    guard = self
                        .shared
                        .condvar
                        .wait(guard)
                        .expect("queue mutex poisoned");
                }
            }
        }
    }

    /// Return the retained stdout history.
    pub fn captured_stdout(&self) -> Vec<Vec<u8>> {
        self.shared
            .queues
            .lock()
            .expect("queue mutex poisoned")
            .stdout_history
            .clone()
            .into_iter()
            .collect()
    }

    fn captured_stdout_raw(&self) -> Vec<u8> {
        self.shared
            .queues
            .lock()
            .expect("queue mutex poisoned")
            .stdout_raw
            .clone()
    }

    /// Return the retained stderr history.
    pub fn captured_stderr(&self) -> Vec<Vec<u8>> {
        if self.config.stderr_mode == StderrMode::Stdout {
            return Vec::new();
        }
        self.shared
            .queues
            .lock()
            .expect("queue mutex poisoned")
            .stderr_history
            .clone()
            .into_iter()
            .collect()
    }

    fn captured_stderr_raw(&self) -> Vec<u8> {
        if self.config.stderr_mode == StderrMode::Stdout {
            return Vec::new();
        }
        self.shared
            .queues
            .lock()
            .expect("queue mutex poisoned")
            .stderr_raw
            .clone()
    }

    /// Return the retained combined stdout/stderr event history.
    pub fn captured_combined(&self) -> Vec<StreamEvent> {
        self.shared
            .queues
            .lock()
            .expect("queue mutex poisoned")
            .combined_history
            .clone()
            .into_iter()
            .collect()
    }

    /// Return the retained byte count for one captured stream.
    pub fn captured_stream_bytes(&self, stream: StreamKind) -> usize {
        if stream == StreamKind::Stderr && self.config.stderr_mode == StderrMode::Stdout {
            return 0;
        }
        let guard = self.shared.queues.lock().expect("queue mutex poisoned");
        match stream {
            StreamKind::Stdout => guard.stdout_history_bytes,
            StreamKind::Stderr => guard.stderr_history_bytes,
        }
    }

    /// Return the retained byte count for combined captured output.
    pub fn captured_combined_bytes(&self) -> usize {
        self.shared
            .queues
            .lock()
            .expect("queue mutex poisoned")
            .combined_history_bytes
    }

    /// Clear retained output history for one stream and return freed bytes.
    pub fn clear_captured_stream(&self, stream: StreamKind) -> usize {
        if stream == StreamKind::Stderr && self.config.stderr_mode == StderrMode::Stdout {
            return 0;
        }
        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");
        match stream {
            StreamKind::Stdout => {
                let released = guard.stdout_history_bytes;
                guard.stdout_history.clear();
                guard.stdout_raw.clear();
                guard.stdout_history_bytes = 0;
                released
            }
            StreamKind::Stderr => {
                let released = guard.stderr_history_bytes;
                guard.stderr_history.clear();
                guard.stderr_raw.clear();
                guard.stderr_history_bytes = 0;
                released
            }
        }
    }

    /// Clear retained combined output history and return freed bytes.
    pub fn clear_captured_combined(&self) -> usize {
        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");
        let released = guard.combined_history_bytes;
        guard.combined_history.clear();
        guard.combined_history_bytes = 0;
        released
    }

    fn build_command(&self) -> Command {
        let command_override = self
            .command_override
            .lock()
            .expect("command override mutex poisoned")
            .take();
        let mut command = match command_override {
            Some(command) => command,
            None => {
                let mut command = match &self.config.command {
                    CommandSpec::Shell(command) => shell_command(command),
                    CommandSpec::Argv(argv) => {
                        let mut command = Command::new(&argv[0]);
                        if argv.len() > 1 {
                            command.args(&argv[1..]);
                        }
                        command
                    }
                };
                if let Some(cwd) = &self.config.cwd {
                    command.current_dir(cwd);
                }
                if let Some(env) = &self.config.env {
                    command.env_clear();
                    command.envs(env.iter().map(|(k, v)| (k, v)));
                }
                command
            }
        };
        let platform_config =
            running_process_platform_internal::platform::process::ProcessCommandConfig {
                creation_flags: self.config.creationflags,
                create_process_group: self.config.create_process_group,
                nice: self.config.nice,
                address_space_limit_bytes: self.config.address_space_limit_bytes,
            };
        let configured = if self.kill_when_owner_dies {
            running_process_platform_internal::platform::process::
                configure_process_command_for_bounded_owner_death(&mut command, platform_config)
        } else {
            running_process_platform_internal::platform::process::configure_process_command(
                &mut command,
                platform_config,
            )
        };
        configured.expect("platform command configuration must be valid");
        command
    }

    fn spawn_reader<R>(
        &self,
        pipe: R,
        source_stream: StreamKind,
        visible_stream: StreamKind,
        on_pipe_done: Box<dyn FnOnce() + Send>,
    ) where
        R: Read + Send + 'static,
    {
        let shared = Arc::clone(&self.shared);
        shared.active_capture_readers.fetch_add(1, Ordering::AcqRel);
        thread::spawn(move || {
            let mut reader = pipe;
            let mut chunk = vec![0_u8; 65536];
            let mut pending = Vec::new();

            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if append_raw(&shared, visible_stream, &chunk[..n]) {
                            let lines = feed_chunk(&mut pending, &chunk[..n]);
                            emit_lines(&shared, visible_stream, lines);
                        } else {
                            pending.clear();
                        }
                    }
                    Err(_) => break,
                }
            }

            if !pending.is_empty() && !shared.capture_overflowed.load(Ordering::Acquire) {
                emit_lines(&shared, visible_stream, vec![std::mem::take(&mut pending)]);
            }

            // Clear the parent-side pipe-handle slot under its mutex
            // before dropping the reader. After this returns,
            // `kill_impl` can no longer try to `CancelIoEx` on us, so
            // it's safe for `reader`'s drop to close the HANDLE.
            on_pipe_done();
            drop(reader);

            let mut guard = shared.queues.lock().expect("queue mutex poisoned");
            match source_stream {
                StreamKind::Stdout => guard.stdout_closed = true,
                StreamKind::Stderr => guard.stderr_closed = true,
            }
            shared.active_capture_readers.fetch_sub(1, Ordering::AcqRel);
            shared.condvar.notify_all();
        });
    }

    fn pipe_done_callback(&self, stream: StreamKind) -> Box<dyn FnOnce() + Send> {
        let cancellation = Arc::clone(&self.capture_cancellation);
        Box::new(move || {
            let stream = match stream {
                StreamKind::Stdout => {
                    running_process_platform_internal::platform::process::CaptureStream::Stdout
                }
                StreamKind::Stderr => {
                    running_process_platform_internal::platform::process::CaptureStream::Stderr
                }
            };
            running_process_platform_internal::platform::process::capture_reader_done(
                &cancellation,
                stream,
            );
        })
    }

    /// Cancel pending capture reads so reader threads return immediately.
    /// Used by `kill_impl` to break the grandchild-orphan deadlock without
    /// waiting on `wait_for_capture_completion_with_deadline`'s safety-net.
    fn cancel_capture_io(&self) {
        crate::rp_rust_debug_scope!("running_process::NativeProcess::cancel_capture_io");
        running_process_platform_internal::platform::process::cancel_capture_reader(
            &self.capture_cancellation,
        );
    }

    fn set_returncode(&self, code: i32) {
        self.shared.returncode.store(code as i64, Ordering::Release);
        self.shared.condvar.notify_all();
    }

    /// Bounded capture drain for the natural-exit and `close` paths
    /// (issue #590, cluster A). Waits at most `kill_drain_deadline` for the
    /// reader threads to flip the closed flags, force-setting them on
    /// timeout so `wait()`/`close()` return in bounded time instead of
    /// wedging in the previously-unbounded `wait_for_capture_completion`.
    /// Unlike `kill_impl` the reader is not cancelled up front — a
    /// short-lived grandchild's output is allowed to drain within the
    /// grace window — but if the window elapses with the pipe still held
    /// open the reader is cancelled to release the leaked thread.
    fn finish_capture_drain(&self) {
        self.finish_capture_drain_with_deadline(kill_drain_deadline());
    }

    fn finish_capture_drain_with_deadline(&self, deadline: Instant) {
        let drained = self.wait_for_capture_completion_with_deadline_impl(deadline);
        if !drained {
            self.cancel_capture_io();
        }
    }

    /// Returns `true` if the reader threads flipped both closed flags on their
    /// own before `deadline`, `false` if the deadline forced completion.
    fn wait_for_capture_completion_with_deadline_impl(&self, deadline: Instant) -> bool {
        crate::rp_rust_debug_scope!(
            "running_process::NativeProcess::wait_for_capture_completion_with_deadline"
        );
        if !self.config.capture {
            return true;
        }
        finalize_capture_completion(&self.shared, deadline)
    }

    fn wait_for_capture_readers_with_deadline(&self, deadline: Instant) -> bool {
        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");
        while self.shared.active_capture_readers.load(Ordering::Acquire) != 0 {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (next_guard, result) = self
                .shared
                .condvar
                .wait_timeout(guard, deadline - now)
                .expect("queue mutex poisoned");
            guard = next_guard;
            if result.timed_out() && self.shared.active_capture_readers.load(Ordering::Acquire) != 0
            {
                return false;
            }
        }
        true
    }
}

/// Cancel any pending blocking `read()` on the parent-side capture pipes
/// so the reader threads' `read()` calls return `ERROR_OPERATION_ABORTED`
/// immediately. Shared by `kill_impl`, `poll`, and the natural-exit
/// waiter thread (issue #590) — anywhere the child is observed to exit
/// while a grandchild may still hold the pipe open.
/// Wait until both capture streams report closed or `deadline` elapses.
/// On deadline, force-set the closed flags (and notify all waiters) so
/// downstream pollers observe EOF instead of blocking forever. Returns
/// `true` if the reader threads flipped the flags on their own, `false`
/// if the deadline forced them. A reader thread that later unblocks and
/// re-sets `closed = true` is a harmless no-op.
fn finalize_capture_completion(shared: &SharedState, deadline: Instant) -> bool {
    let mut guard = shared.queues.lock().expect("queue mutex poisoned");
    while !(guard.stdout_closed && guard.stderr_closed) {
        let now = Instant::now();
        if now >= deadline {
            guard.stdout_closed = true;
            guard.stderr_closed = true;
            shared.condvar.notify_all();
            return false;
        }
        let (next_guard, result) = shared
            .condvar
            .wait_timeout(guard, deadline - now)
            .expect("queue mutex poisoned");
        guard = next_guard;
        if result.timed_out() && !(guard.stdout_closed && guard.stderr_closed) {
            guard.stdout_closed = true;
            guard.stderr_closed = true;
            shared.condvar.notify_all();
            return false;
        }
    }
    true
}

fn emit_lines(shared: &Arc<SharedState>, stream: StreamKind, lines: Vec<Vec<u8>>) {
    if lines.is_empty() || shared.capture_overflowed.load(Ordering::Acquire) {
        return;
    }
    let mut guard = shared.queues.lock().expect("queue mutex poisoned");
    if shared.capture_overflowed.load(Ordering::Acquire) {
        return;
    }
    for line in lines {
        let line_len = line.len();
        match stream {
            StreamKind::Stdout => {
                guard.stdout_history_bytes += line_len;
                guard.stdout_history.push_back(line.clone());
                guard.stdout_queue.push_back(line.clone());
            }
            StreamKind::Stderr => {
                guard.stderr_history_bytes += line_len;
                guard.stderr_history.push_back(line.clone());
                guard.stderr_queue.push_back(line.clone());
            }
        }
        let event = StreamEvent { stream, line };
        guard.combined_history_bytes += line_len;
        guard.combined_history.push_back(event.clone());
        guard.combined_queue.push_back(event);
    }
    shared.condvar.notify_all();
}

fn append_raw(shared: &Arc<SharedState>, stream: StreamKind, chunk: &[u8]) -> bool {
    if chunk.is_empty() {
        return true;
    }
    let mut guard = shared.queues.lock().expect("queue mutex poisoned");
    let accepted = match shared.capture_limit {
        Some(limit) => {
            let retained = guard
                .stdout_raw
                .len()
                .saturating_add(guard.stderr_raw.len());
            chunk.len().min(limit.saturating_sub(retained))
        }
        None => chunk.len(),
    };
    match stream {
        StreamKind::Stdout => guard.stdout_raw.extend_from_slice(&chunk[..accepted]),
        StreamKind::Stderr => guard.stderr_raw.extend_from_slice(&chunk[..accepted]),
    }
    if accepted != chunk.len() {
        shared.capture_overflowed.store(true, Ordering::Release);
        false
    } else {
        true
    }
}

mod bounded;
pub use bounded::{
    run_command, run_command_bounded, run_std_command_bounded,
    run_std_command_bounded_with_options, BoundedRunOptions,
};

pub(crate) fn shell_command(command: &str) -> Command {
    running_process_platform_internal::platform::process::compat_shell_command(command)
}

#[cfg(test)]
mod tests;
