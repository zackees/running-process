//! Cross-platform process execution, process-tree control, PTY handling, and
//! broker integration primitives.
//!
//! The crate exposes a synchronous process API through [`NativeProcess`], a
//! contained process-group helper through [`ContainedProcessGroup`], low-level
//! spawn helpers through [`spawn()`] and [`spawn_daemon`], and optional
//! daemon/broker modules behind feature flags.
// #1101: environment reads go through declared variables; see the
// `running_process_env_direct` Dylint lint.
#![cfg_attr(
    dylint_lib = "running_process_env_literal",
    deny(running_process_env_direct)
)]

use std::collections::VecDeque;
use std::io::Read;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::observer::{ObserverEmitter, ProcessWatchEmitter};

/// Explicit foreground commands preserving caller-controlled native launch state.
pub use running_process_platform_internal::foreground;
pub(crate) use running_process_platform_internal::platform;

mod actor_runtime;
pub mod ape;
#[cfg(feature = "async-process")]
mod async_process;
#[cfg(feature = "async-process")]
mod blocking_island;
mod child_actor;
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
#[cfg(feature = "window-icon")]
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
/// Frozen v1 semantic registration compatibility contract.
#[cfg(feature = "daemon-registration")]
pub mod daemon_registration_compat;
/// Frozen v2 service-definition registration writer substrate.
///
/// This direct persistence surface owns the established `.servicedef.v2`
/// layout, generated service definition, validation, and owner-private file
/// behavior. It deliberately does not select v2 manifests, a loader, broker
/// negotiation, endpoint transport, identity, or an async runtime.
#[cfg(feature = "daemon-registration-v2")]
pub mod daemon_registration_v2;
/// Limited shared-broker v2 registration compatibility contract.
#[cfg(feature = "daemon-registration-v2")]
pub mod daemon_registration_v2_compat;
// The two registration writer features share only the small path, name, error,
// and owner-private-directory substrate. Keeping it separate from either
// public module prevents v2 persistence from selecting v1's SHA-256 manifest
// support, while retaining exact v1 type identity through re-exports.
/// Canonical semantic v1 frame compatibility contract, retaining raw values.
#[cfg(feature = "frame-v1-codec")]
pub mod daemon_frame_v1;
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
#[cfg(feature = "independent-spawn")]
pub mod independent_spawn;
pub mod process_tree;
#[cfg(feature = "pty")]
/// PTY-backed process APIs.
pub mod pty;
mod public_symbols;
mod rust_debug;
pub mod spawn;
mod spawn_contract;
pub use spawn_contract::{IndependentBackend, SpawnLifetime, SpawnMode, SpawnOptions};
#[cfg(feature = "independent-spawn")]
mod spawn_dispatch;
#[cfg(feature = "independent-spawn")]
pub use spawn_dispatch::{spawn_with_options, SpawnExit, SpawnHandle};
pub mod systemd_killmode;
#[cfg(feature = "terminal-graphics")]
pub mod terminal_graphics;
mod types;
#[cfg(unix)]
mod unix;
mod windows;

#[cfg(feature = "async-process")]
pub use async_process::{
    AsyncCapturedOutput, AsyncProcess, AsyncProcessBuilder, AsyncProcessSession,
    AsyncProcessSessionChunk, AsyncProcessSessionControl, AsyncProcessSessionEvent,
    AsyncProcessSessionOptions, AsyncProcessSessionOutput, AsyncStdio, ProcessTreeKill,
};
pub use console_detect::{monitor_console_windows, ConsoleWindowInfo};
pub use containment::{ContainedProcessGroup, ORIGINATOR_ENV_VAR};
// #891: content-hash primitive for dev daemon-identity isolation.
#[cfg(feature = "client")]
pub use content_hash::{blake3_file, daemon_identity_stamp, daemon_identity_stamp_env};
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
/// Canonical native process-inspection errors, preserving their host detail.
pub use running_process_platform_internal::platform::process::{
    ProcessInspectError, ProcessInspectErrorKind,
};
/// Resolve the current executable image for a live PID.
pub use running_process_platform_internal::process_executable_path;
/// Compare executable path spellings using the host-native policy.
pub use running_process_platform_internal::process_same_executable_path;
/// Retained native process-liveness observation.
pub use running_process_platform_internal::ProcessLiveness;
pub use rust_debug::{render_rust_debug_traces, RustDebugScopeGuard};
pub use spawn::{
    spawn, spawn_daemon, spawn_daemon_breaking_away_from_job,
    spawn_daemon_breaking_away_with_env_policy, spawn_daemon_with_clear_env,
    spawn_daemon_with_env_policy, spawn_daemon_with_environment,
    spawn_daemon_with_explicit_environment, spawn_daemon_with_stdio,
    spawn_daemon_with_stdio_and_env_policy, spawn_with_env_policy, spawn_with_environment,
    spawn_with_explicit_environment, DaemonChild, DaemonStdio, DaemonStdioSource,
    EnvironmentPolicy, SpawnStdio, SpawnedChild, SpawnedChildControl, StdioSource, SyncEnvironment,
    DAEMON_MARKER_ENV_VAR,
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
#[cfg(feature = "window-icon")]
pub use window_icon::{
    host_icon_support, icon_support, set_host_icon, set_icon, IconError, IconScope, IconSource,
    IconSupport, StockIcon,
};

pub(crate) use helpers::child_try_wait_error_is_retryable;
#[cfg(test)]
pub(crate) use helpers::exit_code;
pub(crate) use helpers::{feed_chunk, kill_drain_deadline, log_spawned_child_pid};
/// Convert a native process exit status to the portable integer convention.
pub use running_process_platform_internal::exit_code as native_exit_code;
pub use running_process_platform_internal::ProcessPriority;
#[cfg(feature = "async-process")]
pub use running_process_platform_internal::SpawnAdmission;
#[cfg(unix)]
pub use unix::{unix_set_priority, unix_signal_process, unix_signal_process_group, UnixSignal};
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
    /// Byte-exact stream chunks. Unlike the logical line queues these retain
    /// delimiters, unterminated tails, and non-UTF-8 bytes; callers consume
    /// them with `drain_stream_raw`.
    stdout_raw: VecDeque<Vec<u8>>,
    stderr_raw: VecDeque<Vec<u8>>,
    stdout_raw_bytes: usize,
    stderr_raw_bytes: usize,
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
    /// Updated by the child actor — reading is lock-free.
    returncode: AtomicI64,
    /// Phase 1 of #221: optional lifecycle-event emitter. `None` means
    /// observation is off (the off-by-default path), so the lifecycle
    /// hooks are inert. When `Some`, `started` is emitted once at spawn
    /// and `exited` exactly once on the first returncode transition.
    observer: Option<ObserverEmitter>,
    /// Guards against emitting more than one `exited` event when several
    /// code paths (the child actor serving its tick, `poll`, `kill`) race to record the exit.
    observer_exit_emitted: AtomicBool,
    /// #850: exit publication for waiters that run on the actor runtime.
    /// Mirrors `returncode`; every write goes through [`Self::record_exit`].
    exit_code: tokio::sync::watch::Sender<Option<i32>>,
}

/// The child owned by a started [`NativeProcess`]: the platform's non-Tokio
/// backend (a std or exact-trace child, observed by `try_wait` polling from
/// the actor runtime), which also owns the Windows per-spawn Job Object and
/// the capture cancellation for its pipes (#850).
type ChildState = running_process_platform_internal::platform::process::PlatformStdChild;

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

fn cleanup_child_after_start_error(child: ChildState) {
    child.discard_after_start_error();
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
            exit_code: tokio::sync::watch::Sender::new(None),
        }
    }

    /// Publish the exit code to every observer: lock-free readers of
    /// `returncode`, condvar waiters, and actor-runtime waiters.
    fn record_exit(&self, code: i32) {
        self.returncode.store(code as i64, Ordering::Release);
        self.exit_code.send_replace(Some(code));
        self.condvar.notify_all();
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
    /// Set once by `start`; the child itself lives in its actor.
    child: std::sync::OnceLock<child_actor::ChildHandle>,
    /// Serializes concurrent `start` calls; never held by any other path.
    start_gate: Mutex<()>,
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
            child: std::sync::OnceLock::new(),
            start_gate: Mutex::new(()),
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
        let _gate = self.start_gate.lock().expect("start gate poisoned");
        if self.child.get().is_some() {
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
                Ok(child) => ChildState::from_exact_trace(child, self.config.create_process_group),
                Err(error) => {
                    if let Some(watch) = self.process_watch.as_ref() {
                        watch.close();
                    }
                    return Err(ProcessError::Spawn(error));
                }
            }
        } else {
            ChildState::from_std(
                running_process_platform_internal::platform::ape::spawn_std(
                    &mut command,
                    |command| command.spawn(),
                )
                .map_err(ProcessError::Spawn)?,
                self.config.create_process_group,
            )
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
        // Hosts without Job Objects answer `Unsupported`, which means there is
        // nothing to contain; an exact-trace child has no standard handle to
        // assign.
        let job_result = child.std_child().map(|standard_child| {
            let descendant_sink = self
                .shared
                .observer
                .as_ref()
                .and_then(|e| e.descendant_sink());
            public_symbols::rp_assign_child_to_windows_kill_on_close_job_with_observer_public(
                standard_child,
                descendant_sink,
                self.process_watch.clone(),
                standard_child.id(),
                self.config.address_space_limit_bytes,
            )
        });
        match job_result {
            None => {}
            Some(Ok(job)) => child.attach_job(job),
            Some(Err(error)) if error.kind() == std::io::ErrorKind::Unsupported => {}
            Some(Err(error)) => {
                if let Some(watch) = self.process_watch.as_ref() {
                    watch.close();
                }
                cleanup_child_after_start_error(child);
                return Err(ProcessError::Spawn(error));
            }
        }
        if !exact_trace {
            descendant_monitor::start(
                child.id(),
                self.shared.observer.as_ref(),
                self.process_watch.as_ref(),
            );
        }
        if self.config.capture {
            let readers = match child.prepare_capture(&self.capture_cancellation) {
                Ok(readers) => readers,
                Err(error) => {
                    cleanup_child_after_start_error(child);
                    return Err(ProcessError::Spawn(error));
                }
            };
            let (stdout, stderr) = (readers.stdout, readers.stderr);
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
        let handle = child_actor::spawn(
            child,
            Arc::clone(&self.shared),
            self.config.capture,
            Arc::clone(&self.capture_cancellation),
        );
        let _ = self.child.set(handle);
        Ok(())
    }

    /// Write bytes to the child's stdin and then close stdin.
    pub fn write_stdin(&self, data: &[u8]) -> Result<(), ProcessError> {
        if self.child.get().is_none() {
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
        if self.child.get().is_none() {
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
        if self.child.get().is_none() {
            return Err(ProcessError::NotRunning);
        }
        drop(self.stdin.lock().expect("stdin mutex poisoned").take());
        Ok(())
    }

    /// Check whether the child has exited without blocking.
    ///
    /// Returns `Ok(None)` while the process is still running.
    pub fn poll(&self) -> Result<Option<i32>, ProcessError> {
        // Fast path: check atomic set by the child actor.
        if let Some(code) = self.returncode() {
            return Ok(Some(code));
        }
        let Some(child) = self.child.get() else {
            return Ok(self.returncode());
        };
        // The actor publishes the exit (returncode, watch, lifecycle event)
        // when this is the check that finds it.
        child.try_wait().map_err(ProcessError::Io)
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
        if self.child.get().is_none() {
            return self.returncode().ok_or(ProcessError::NotRunning);
        }
        // Fast path: already exited.
        if let Some(code) = self.returncode() {
            self.finish_capture_drain();
            return Ok(code);
        }
        // A short timed wait must not depend on the child actor's tick. That
        // task runs on the runtime's timer, whose granularity is coarse on some
        // hosts (about 15 ms on Windows), so a child that has already exited can
        // stay unobserved for longer than a caller's grace period -- which made
        // `stream_iter` emit a second terminal event when its 10 ms grace lapsed
        // right after EOF. For the first stretch of a timed wait, check the
        // child directly on the calling thread, where a sleep is precise.
        let mut timeout = timeout;
        if let Some(limit) = timeout {
            let fine = limit.min(SHORT_WAIT_DIRECT_POLL);
            let deadline = Instant::now() + fine;
            loop {
                // A failed check is treated as "still running": the lifecycle
                // tick, or the timeout below, decides what happens next.
                if let Some(child) = self.child.get() {
                    let _ = child.try_wait();
                }
                if let Some(code) = self.returncode() {
                    self.finish_capture_drain();
                    return Ok(code);
                }
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                thread::sleep((deadline - now).min(Duration::from_millis(1)));
            }
            if let Some(code) = self.returncode() {
                self.finish_capture_drain();
                return Ok(code);
            }
            let remaining = limit.saturating_sub(fine);
            if remaining.is_zero() {
                return Err(ProcessError::Timeout);
            }
            timeout = Some(remaining);
        }
        // #850: the exit is published by the child actor on the actor
        // runtime. `block_on_anywhere` is safe from a Tokio worker too, so a
        // sync caller inside async code keeps working rather than erroring.
        let mut exit = self.shared.exit_code.subscribe();
        let outcome = actor_runtime::block_on_anywhere(async move {
            let exited = async move {
                exit.wait_for(Option::is_some)
                    .await
                    .ok()
                    .and_then(|code| *code)
            };
            match timeout {
                Some(limit) => tokio::time::timeout(limit, exited).await.ok(),
                None => Some(exited.await),
            }
        });
        match outcome {
            None => Err(ProcessError::Timeout),
            // The sender lives in `self.shared`, so it cannot close while
            // `self` is borrowed; treat that impossibility as not running.
            Some(None) => Err(ProcessError::NotRunning),
            Some(Some(code)) => {
                self.finish_capture_drain();
                Ok(code)
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
        let deadline = kill_drain_deadline();
        let child = self.child.get().ok_or(ProcessError::NotRunning)?;
        // The actor checks for an exit first and otherwise signals the child
        // (group-wide when it leads its own group and the host can).
        let already_reaped = match child.kill().map_err(ProcessError::Io)? {
            child_actor::KillOutcome::AlreadyExited(code) => Some(code),
            child_actor::KillOutcome::Signalled => None,
        };

        // Wake capture readers immediately after the kill. In particular, this
        // prevents a surviving pipe-owning descendant (FastLED Bug B: `uv`
        // spawns a `python` grandchild that inherits the pipe and outlives it)
        // from extending the bounded reap window, and wakes a blocked
        // `read()` in microseconds rather than at the drain deadline.
        self.cancel_capture_io();
        // The actor publishes the exit and the lifecycle `exited` event
        // (Phase 1 of #221: a killed child still produces one); all this has
        // to do is give it the bounded window to observe the reap.
        if already_reaped.is_none() {
            self.await_exit_until(deadline);
        }
        // Synchronize with the per-stream reader threads so that by the time
        // kill() returns, the capture queues have flipped from "blocked on
        // read" to "closed" and downstream pollers (e.g. take_combined_line)
        // observe EOS instead of timeout. The deadline remains a safety net
        // if the platform wake mechanism does not fire.
        public_symbols::rp_native_process_wait_for_capture_completion_with_deadline_public(
            self, deadline,
        );
        Ok(())
    }

    /// Wait for the child actor to publish the exit, up to `deadline`. A last
    /// `try_wait` through the actor covers a lifecycle tick that has already
    /// stopped (for instance after a terminal `try_wait` error).
    fn await_exit_until(&self, deadline: Instant) -> Option<i32> {
        let mut exit = self.shared.exit_code.subscribe();
        let remaining = deadline.saturating_duration_since(Instant::now());
        let published = actor_runtime::block_on_anywhere(async move {
            tokio::time::timeout(remaining, exit.wait_for(Option::is_some))
                .await
                .ok()
                .and_then(|seen| seen.ok().and_then(|code| *code))
        });
        published.or_else(|| self.child.get()?.try_wait().ok().flatten())
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
        if self.child.get().is_none() {
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
        self.child.get().map(child_actor::ChildHandle::pid)
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

    /// Consume and return all byte-exact output currently captured for one
    /// stream.
    ///
    /// This is independent of the logical-line queues used by
    /// [`Self::read_stream`] and [`Self::drain_stream`]. It preserves CRLF/LF
    /// delimiters, unterminated tails, and non-UTF-8 bytes exactly as accepted
    /// from the pipe reader. Calling it after EOF returns an empty vector once
    /// the stream has been drained.
    pub fn drain_stream_raw(&self, stream: StreamKind) -> Vec<u8> {
        if stream == StreamKind::Stderr && self.config.stderr_mode == StderrMode::Stdout {
            return Vec::new();
        }
        let mut guard = self.shared.queues.lock().expect("queue mutex poisoned");
        match stream {
            StreamKind::Stdout => {
                let mut output = Vec::with_capacity(guard.stdout_raw_bytes);
                for chunk in guard.stdout_raw.drain(..) {
                    output.extend_from_slice(&chunk);
                }
                guard.stdout_raw_bytes = 0;
                output
            }
            StreamKind::Stderr => {
                let mut output = Vec::with_capacity(guard.stderr_raw_bytes);
                for chunk in guard.stderr_raw.drain(..) {
                    output.extend_from_slice(&chunk);
                }
                guard.stderr_raw_bytes = 0;
                output
            }
        }
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
        let guard = self.shared.queues.lock().expect("queue mutex poisoned");
        guard.stdout_raw.iter().flatten().copied().collect()
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
        let guard = self.shared.queues.lock().expect("queue mutex poisoned");
        guard.stderr_raw.iter().flatten().copied().collect()
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
    ///
    /// This releases both the logical-line history and the byte-exact queue
    /// behind [`Self::drain_stream_raw`], so it stays the single memory-release
    /// valve for a captured stream. The returned count is the logical-line
    /// history only, unchanged. A caller consuming byte-exact output should
    /// drain it with [`Self::drain_stream_raw`] — which frees it too — rather
    /// than interleaving this call.
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
                guard.stdout_raw_bytes = 0;
                guard.stdout_history_bytes = 0;
                released
            }
            StreamKind::Stderr => {
                let released = guard.stderr_history_bytes;
                guard.stderr_history.clear();
                guard.stderr_raw.clear();
                guard.stderr_raw_bytes = 0;
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
                // The child's PATH to set last, when an APE launch puts its
                // loader's directory first on it.
                let mut ape_path = None;
                let mut command = match &self.config.command {
                    CommandSpec::Shell(command) => shell_command(command),
                    CommandSpec::Argv(argv) => {
                        // An APE image runs through its planned loader on a
                        // host that cannot exec it (see `crate::ape`).
                        let options = platform::ape::ApeOptions::with_overrides(
                            self.config.env.is_some(),
                            self.config.env.iter().flatten().map(|(key, value)| {
                                (std::ffi::OsStr::new(key), Some(std::ffi::OsStr::new(value)))
                            }),
                        );
                        match platform::ape::plan_launch(
                            std::ffi::OsStr::new(&argv[0]),
                            self.config.cwd.as_deref(),
                            &options,
                        ) {
                            Some(launch) => {
                                ape_path = launch.child_path(options.path.as_deref());
                                let mut command = Command::new(&launch.loader);
                                command.args(launch.args(&argv[1..]));
                                command
                            }
                            None => {
                                let mut command = Command::new(&argv[0]);
                                command.args(&argv[1..]);
                                command
                            }
                        }
                    }
                };
                if let Some(cwd) = &self.config.cwd {
                    command.current_dir(cwd);
                }
                if let Some(env) = &self.config.env {
                    command.env_clear();
                    command.envs(env.iter().map(|(k, v)| (k, v)));
                }
                if let Some(path) = ape_path {
                    command.env("PATH", path);
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

    #[cfg(test)]
    fn set_returncode(&self, code: i32) {
        self.shared.record_exit(code);
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

/// How long a timed `wait` checks the child directly before falling back to
/// the child actor's lifecycle tick. Long enough to cover the coarsest timer tick a
/// host has, short enough that the polling never becomes the steady-state cost.
const SHORT_WAIT_DIRECT_POLL: Duration = Duration::from_millis(50);

/// Cancel any pending blocking `read()` on the parent-side capture pipes
/// so the reader threads' `read()` calls return `ERROR_OPERATION_ABORTED`
/// immediately. Shared by `kill_impl`, `poll`, and the natural-exit
/// child actor (issue #590) — anywhere the child is observed to exit
/// while a grandchild may still hold the pipe open.
/// Wait until both capture streams report closed or `deadline` elapses.
/// On deadline, force-set the closed flags (and notify all waiters) so
/// downstream pollers observe EOF instead of blocking forever. Returns
/// `true` if the reader threads flipped the flags on their own, `false`
/// if the deadline forced them. A reader thread that later unblocks and
/// re-sets `closed = true` is a harmless no-op.
/// Timer-driven twin of [`finalize_capture_completion`] for the actor
/// runtime, where parking on the queue condvar would block a worker.
pub(crate) async fn finalize_capture_completion_async(
    shared: &SharedState,
    deadline: Instant,
) -> bool {
    loop {
        {
            let mut guard = shared.queues.lock().expect("queue mutex poisoned");
            if guard.stdout_closed && guard.stderr_closed {
                return true;
            }
            if Instant::now() >= deadline {
                guard.stdout_closed = true;
                guard.stderr_closed = true;
                shared.condvar.notify_all();
                return false;
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(remaining.min(Duration::from_millis(10))).await;
    }
}

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
                .stdout_raw_bytes
                .saturating_add(guard.stderr_raw_bytes);
            chunk.len().min(limit.saturating_sub(retained))
        }
        None => chunk.len(),
    };
    if accepted != 0 {
        let accepted_chunk = chunk[..accepted].to_vec();
        match stream {
            StreamKind::Stdout => {
                guard.stdout_raw_bytes += accepted;
                guard.stdout_raw.push_back(accepted_chunk);
            }
            StreamKind::Stderr => {
                guard.stderr_raw_bytes += accepted;
                guard.stderr_raw.push_back(accepted_chunk);
            }
        }
    }
    if accepted != chunk.len() {
        shared.capture_overflowed.store(true, Ordering::Release);
        false
    } else {
        shared.condvar.notify_all();
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
