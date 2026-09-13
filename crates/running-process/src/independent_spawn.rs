//! Explicit containment placement for detached daemons (#1202).
//!
//! Detaching a process changes terminal/lifetime behavior, not Linux cgroup
//! or Windows Job Object membership. `Independent` therefore requires an
//! external launcher and never degrades to the direct inherited path.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Detached daemon handle returned by the canonical independent-spawn APIs.
pub use crate::DaemonChild;

/// Requested resource-containment placement.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SpawnMode {
    /// Existing direct spawn behavior; this is the compatibility default.
    #[default]
    Inherited,
    /// Require verified placement outside the caller's resource group.
    Independent,
}

/// Inspectable daemon command accepted by either containment mode.
///
/// Stdin, stdout and stderr are null. Inherited handles, sockets, pipes and
/// native command hooks are deliberately not representable: an external
/// scheduler cannot preserve them. There is no conversion from an opaque
/// [`Command`], whose stdio and native hooks cannot be inspected.
///
/// Debug output omits the program, arguments and environment values.
pub struct DaemonSpawnRequest {
    command: Command,
    clear_environment: bool,
    invalid_input: bool,
}

impl std::fmt::Debug for DaemonSpawnRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DaemonSpawnRequest")
            .field("clear_environment", &self.clear_environment)
            .finish_non_exhaustive()
    }
}

impl DaemonSpawnRequest {
    /// Start a null-stdio daemon request without native launch hooks.
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        let program = program.as_ref();
        Self {
            command: Command::new(program),
            clear_environment: false,
            invalid_input: program.is_empty() || invalid_native_string(program),
        }
    }

    /// Append one literal argument, without shell parsing.
    pub fn arg(&mut self, argument: impl AsRef<OsStr>) -> &mut Self {
        self.invalid_input |= invalid_native_string(argument.as_ref());
        self.command.arg(argument);
        self
    }

    /// Append literal arguments, preserving their order.
    pub fn args<I, S>(&mut self, arguments: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for argument in arguments {
            self.arg(argument);
        }
        self
    }

    /// Set the daemon's working directory.
    pub fn current_dir(&mut self, directory: impl AsRef<Path>) -> &mut Self {
        self.invalid_input |= invalid_native_string(directory.as_ref().as_os_str());
        self.command.current_dir(directory);
        self
    }

    /// Add or replace an explicit environment variable.
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.invalid_input |=
            invalid_environment_key(key.as_ref()) || invalid_native_string(value.as_ref());
        self.command.env(key, value);
        self
    }

    /// Remove a variable from the inherited environment.
    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.invalid_input |= invalid_environment_key(key.as_ref());
        self.command.env_remove(key);
        self
    }

    /// Clear previous overrides and disable ambient environment inheritance.
    /// Later calls to [`Self::env`] still add explicit variables.
    pub fn env_clear(&mut self) -> &mut Self {
        self.command.env_clear();
        self.clear_environment = true;
        self
    }
}

fn invalid_native_string(value: &OsStr) -> bool {
    // Inspect before passing to Command: its invalid-input flag is private,
    // and getters are not a lossless representation of a failed setter.
    value.as_encoded_bytes().contains(&0)
}

fn invalid_environment_key(key: &OsStr) -> bool {
    if key.is_empty() || invalid_native_string(key) {
        return true;
    }
    #[cfg(unix)]
    if key.as_encoded_bytes().contains(&b'=') {
        return true;
    }
    false
}

/// Cooperative cancellation for a bounded independent launch.
#[derive(Clone, Debug, Default)]
pub struct IndependentSpawnCancellation(Arc<AtomicBool>);
impl IndependentSpawnCancellation {
    /// Create an uncancelled token.
    pub fn new() -> Self {
        Self::default()
    }
    /// Request cancellation.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    /// Whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Explicit policy for [`spawn_daemon_request`] and [`spawn_daemon_with_options`].
#[derive(Clone, Debug)]
pub struct IndependentSpawnOptions {
    /// Requested mode. Defaults to [`SpawnMode::Inherited`].
    pub mode: SpawnMode,
    /// For Independent mode, snapshot the caller environment before applying
    /// explicit command overrides/removals. Set false for an empty base.
    /// A structured request's `env_clear()` always disables inheritance,
    /// regardless of this option. Inherited mode preserves the existing
    /// daemon environment policy instead.
    pub inherit_environment: bool,
    /// Bounded manager acknowledgement/readiness wait.
    pub readiness_timeout: Duration,
    /// Optional cooperative cancellation token.
    pub cancellation: Option<IndependentSpawnCancellation>,
}
impl Default for IndependentSpawnOptions {
    fn default() -> Self {
        Self {
            mode: SpawnMode::Inherited,
            inherit_environment: true,
            readiness_timeout: Duration::from_secs(10),
            cancellation: None,
        }
    }
}

/// A verified native external-launch backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndependentSpawnBackend {
    SystemdTransientService,
    WindowsTaskScheduler,
    ExternalBroker,
}

/// Runtime availability. `available == false` means `Independent` will fail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndependentSpawnCapability {
    pub available: bool,
    pub backend: Option<IndependentSpawnBackend>,
    /// Safe diagnostic text; it contains no target argv/environment values.
    pub reason: String,
}

/// A strict independent-launch failure; none of these imply inherited spawn.
#[derive(Debug, thiserror::Error)]
pub enum IndependentSpawnError {
    #[error("independent spawning is unsupported: {reason}")]
    Unsupported { reason: String },
    #[error("independent spawning was denied: {reason}")]
    PermissionDenied { reason: String },
    #[error("independent spawn launch failed: {reason}")]
    Launch { reason: String },
    #[error("independent spawn readiness timed out after {timeout:?}: {reason}")]
    Readiness { timeout: Duration, reason: String },
    #[error("independent spawn was cancelled")]
    Cancelled,
    /// Launch failed and external cleanup could not be confirmed. The opaque
    /// manager resource identifies what may require operator reconciliation.
    #[error("{cause}; cleanup of {resource} could not be confirmed: {reason}")]
    CleanupUnconfirmed {
        #[source]
        cause: Box<IndependentSpawnError>,
        resource: String,
        reason: String,
    },
}

/// Inspect the currently usable strict independent backend.
pub fn independent_spawn_capability() -> IndependentSpawnCapability {
    platform::capability()
}

/// Spawn a daemon in `mode`.
///
/// `Inherited` preserves the existing direct daemon behavior. `Independent`
/// rejects opaque commands because their stdio and native hooks cannot be
/// inspected. Use [`spawn_daemon_request`] for independent placement.
pub fn spawn_daemon_with_mode(
    command: &mut Command,
    mode: SpawnMode,
) -> Result<DaemonChild, IndependentSpawnError> {
    spawn_daemon_with_options(
        command,
        &IndependentSpawnOptions {
            mode,
            ..Default::default()
        },
    )
}

/// Spawn with explicit cancellation and readiness policy.
pub fn spawn_daemon_with_options(
    command: &mut Command,
    options: &IndependentSpawnOptions,
) -> Result<DaemonChild, IndependentSpawnError> {
    if cancelled(options) {
        return Err(IndependentSpawnError::Cancelled);
    }
    match options.mode {
        SpawnMode::Inherited => crate::spawn_daemon(command).map_err(|error| IndependentSpawnError::Launch { reason: error.to_string() }),
        SpawnMode::Independent => Err(IndependentSpawnError::Unsupported {
            reason: "opaque Command stdio and native hooks cannot cross a scheduler boundary; use DaemonSpawnRequest with spawn_daemon_request".into(),
        }),
    }
}

/// Spawn a structured null-stdio request in the selected containment mode.
///
/// `Inherited` uses the existing daemon environment policy, unless the request
/// explicitly clears the environment. `Independent` snapshots the environment
/// only when `inherit_environment` is true and the request has not cleared it.
/// No caller-owned descriptors or native hooks are transferred to a scheduler.
pub fn spawn_daemon_request(
    request: &mut DaemonSpawnRequest,
    options: &IndependentSpawnOptions,
) -> Result<DaemonChild, IndependentSpawnError> {
    if cancelled(options) {
        return Err(IndependentSpawnError::Cancelled);
    }
    if request.invalid_input {
        return Err(IndependentSpawnError::Launch {
            reason: "invalid native string in daemon request".into(),
        });
    }
    match options.mode {
        SpawnMode::Inherited => {
            crate::spawn_daemon_with_clear_env(&mut request.command, request.clear_environment)
                .map_err(|error| IndependentSpawnError::Launch {
                    reason: error.to_string(),
                })
        }
        SpawnMode::Independent => {
            let mut effective = options.clone();
            effective.inherit_environment &= !request.clear_environment;
            platform::spawn(&mut request.command, &effective)
        }
    }
}

fn cancelled(options: &IndependentSpawnOptions) -> bool {
    options
        .cancellation
        .as_ref()
        .is_some_and(IndependentSpawnCancellation::is_cancelled)
}

/// Shared helper wire shape: target PID/start key, supervisor PID/start key.
/// Start keys remain platform-native (Linux ticks, Windows creation FILETIME).
fn parse_launch_acknowledgement(text: &str) -> std::io::Result<Option<(u32, u64, u32, u64)>> {
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid helper acknowledgement",
        )
    };
    if text.len() > 128 {
        return Err(invalid());
    }
    // Linux readers can observe the writer before its final newline.
    if !text.ends_with('\n') {
        return Ok(None);
    }
    let mut fields = text.split_whitespace();
    let mut number = || -> std::io::Result<u64> {
        let field = fields.next().ok_or_else(invalid)?;
        if !field.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
        field.parse().map_err(|_| invalid())
    };
    let pid = u32::try_from(number()?).map_err(|_| invalid())?;
    let key = number()?;
    let supervisor = u32::try_from(number()?).map_err(|_| invalid())?;
    let supervisor_key = number()?;
    if fields.next().is_some() || pid == 0 || supervisor == 0 || pid == supervisor {
        return Err(invalid());
    }
    Ok(Some((pid, key, supervisor, supervisor_key)))
}

/// A cleanup acknowledgement is an empty private marker, not arbitrary
/// readable content. Malformed evidence cannot establish successful cleanup.
fn cleanup_acknowledged(marker: Option<String>) -> std::io::Result<bool> {
    match marker {
        None => Ok(false),
        Some(value) if value.is_empty() => Ok(true),
        Some(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid cleanup acknowledgement",
        )),
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use running_process_platform_internal::platform::process::DaemonChildControl;
    use running_process_platform_internal::{
        process_start_key, spawn_independent_helper_child, terminate_independent_scheduler_command,
        StrictProcessHandle,
    };
    use std::io;

    pub(super) fn capability() -> IndependentSpawnCapability {
        if let Err(error) = StrictProcessHandle::open(std::process::id())
            .and_then(|handle| handle.check_signal_permission())
        {
            return IndependentSpawnCapability {
                available: false,
                backend: None,
                reason: format!("reuse-safe process control is unavailable: {error}"),
            };
        }
        // A container broker is deliberately not auto-started: a broker born
        // in this caller's cgroup cannot establish independence.
        if let Some(socket) = crate::env_vars::INDEPENDENT_BROKER.path() {
            let result = (|| -> Result<(), IndependentSpawnError> {
                let mut stream = crate::independent_broker_transport::connect(
                    std::path::Path::new(&socket),
                    Instant::now(),
                    Duration::from_secs(2),
                    None,
                )
                .map_err(classify_io)?;
                stream.set_nonblocking(false).map_err(classify_io)?;
                let peer =
                    crate::independent_broker_transport::peer_pid(&stream).map_err(classify_io)?;
                verify_separate_cgroup(peer)?;
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .map_err(classify_io)?;
                crate::independent_broker_transport::read_greeting(&mut stream).map_err(classify_io)
            })();
            return IndependentSpawnCapability {
                available: result.is_ok(),
                backend: Some(IndependentSpawnBackend::ExternalBroker),
                reason: match result {
                    Ok(()) => "same-user external broker protocol and placement verified".into(),
                    Err(error) => error.to_string(),
                },
            };
        }
        if helper_path().is_err() {
            return IndependentSpawnCapability {
                available: false,
                backend: None,
                reason: "independent helper executable is unavailable".into(),
            };
        }
        // Discard manager output: never collect or expose its environment.
        // A reachable executable is insufficient; bound the live manager probe.
        match probe_manager(Duration::from_secs(2)) {
            Ok(true) => IndependentSpawnCapability {
                available: true,
                backend: Some(IndependentSpawnBackend::SystemdTransientService),
                reason: "user systemd manager is reachable".into(),
            },
            Ok(false) => IndependentSpawnCapability {
                available: false,
                backend: None,
                reason: "systemd user manager probe failed or timed out".into(),
            },
            Err(error) => IndependentSpawnCapability {
                available: false,
                backend: None,
                reason: format!("systemctl unavailable: {error}"),
            },
        }
    }

    fn probe_manager(timeout: Duration) -> io::Result<bool> {
        let mut command = Command::new("systemctl");
        command.args(["--user", "show", "--property=Version", "--value"]);
        let mut child = spawn_independent_helper_child(&mut command)?;
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status.success()),
                Ok(None) if started.elapsed() < timeout => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                result => {
                    terminate_independent_scheduler_command(child)?;
                    return result.map(|_| false);
                }
            }
        }
    }

    pub(super) fn spawn(
        command: &mut Command,
        options: &IndependentSpawnOptions,
    ) -> Result<DaemonChild, IndependentSpawnError> {
        let started = Instant::now();
        let broker = crate::env_vars::INDEPENDENT_BROKER.path();
        if options.readiness_timeout.is_zero() {
            return Err(IndependentSpawnError::Readiness {
                timeout: options.readiness_timeout,
                reason: "launch deadline already expired".into(),
            });
        }
        // Check kernel/seccomp support before submitting anything externally.
        StrictProcessHandle::open(std::process::id())
            .and_then(|handle| handle.check_signal_permission())
            .map_err(classify_io)?;
        // Preserve bytes and keep secrets out of manager argv. The scheduler
        // helper consumes this owner-private artifact and writes a bounded
        // acknowledgement; direct systemd-run cannot safely carry this data.
        let helper = if broker.is_none() {
            helper_path().map_err(classify_io)?
        } else {
            std::path::PathBuf::new()
        };
        let request = crate::independent_transport::LaunchRequest::from_command(
            command,
            options.inherit_environment,
        )
        .map_err(classify_io)?;
        let request_path =
            crate::independent_transport::write_private_request(&request).map_err(classify_io)?;
        let acknowledgement = request_path.with_file_name("ack");
        let unit = format!(
            "running-process-independent-{}-{}.service",
            std::process::id(),
            unique_suffix()
        );
        let mut manager = Command::new("systemd-run");
        manager.args([
            "--user",
            "--quiet",
            "--collect",
            "--service-type=exec",
            "--unit",
            &unit,
        ]);
        manager.args([
            "--property=StandardInput=null",
            "--property=StandardOutput=null",
            "--property=StandardError=null",
        ]);
        manager
            .arg("--")
            .arg(helper)
            .arg(&request_path)
            .arg(&acknowledgement)
            .arg(
                options
                    .readiness_timeout
                    .as_millis()
                    .saturating_add(1000)
                    .min(u64::MAX as u128)
                    .to_string(),
            )
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if cancelled(options) || started.elapsed() >= options.readiness_timeout {
            let _ = std::fs::remove_file(&request_path);
            if let Some(parent) = request_path.parent() {
                let _ = std::fs::remove_dir(parent);
            }
            return Err(if cancelled(options) {
                IndependentSpawnError::Cancelled
            } else {
                IndependentSpawnError::Readiness {
                    timeout: options.readiness_timeout,
                    reason: "launch deadline expired before submission".into(),
                }
            });
        }
        let mut broker_may_have_submitted = false;
        let launch = if let Some(socket) = broker.as_deref() {
            submit_broker(
                socket,
                &request_path,
                options,
                started,
                &mut broker_may_have_submitted,
            )
            .map(|()| None)
            .map_err(|error| {
                if cancelled(options) {
                    IndependentSpawnError::Cancelled
                } else if started.elapsed() >= options.readiness_timeout {
                    IndependentSpawnError::Readiness {
                        timeout: options.readiness_timeout,
                        reason: "external broker submission deadline expired".into(),
                    }
                } else {
                    error
                }
            })
        } else {
            spawn_independent_helper_child(&mut manager)
                .map(Some)
                .map_err(classify_io)
        };
        let mut launcher = match launch {
            Ok(launcher) => launcher,
            Err(error) => {
                if broker_may_have_submitted {
                    if let Err(cleanup) = cancel_broker_request(&request_path) {
                        return Err(IndependentSpawnError::CleanupUnconfirmed {
                            cause: Box::new(error),
                            resource: request_path.display().to_string(),
                            reason: cleanup.to_string(),
                        });
                    }
                }
                let _ = std::fs::remove_file(&request_path);
                if let Some(parent) = request_path.parent() {
                    let _ = std::fs::remove_dir(parent);
                }
                return Err(error);
            }
        };
        let result = (|| {
            while let Some(launcher) = launcher.as_mut() {
                if cancelled(options) {
                    return Err(IndependentSpawnError::Cancelled);
                }
                if started.elapsed() >= options.readiness_timeout {
                    return Err(IndependentSpawnError::Readiness {
                        timeout: options.readiness_timeout,
                        reason: "external launcher did not acknowledge in time".into(),
                    });
                }
                match launcher.try_wait().map_err(classify_io)? {
                    Some(status) if status.success() => break,
                    Some(_) => {
                        return Err(IndependentSpawnError::Launch {
                            reason: "systemd rejected independent helper launch".into(),
                        })
                    }
                    None => std::thread::sleep(Duration::from_millis(10)),
                }
            }
            loop {
                if cancelled(options) {
                    return Err(IndependentSpawnError::Cancelled);
                }
                if let Some((pid, start_key, supervisor_pid, supervisor_key)) =
                    read_acknowledgement(&acknowledgement).map_err(classify_io)?
                {
                    // The supervisor owns exit-status reporting and the acceptance
                    // protocol. A target outside containment is insufficient if
                    // caller teardown can still kill its supervisor.
                    let supervisor = open_independent_identity(supervisor_pid, supervisor_key)?;
                    let identity = open_independent_identity(pid, start_key)?;
                    if cancelled(options) {
                        return Err(IndependentSpawnError::Cancelled);
                    }
                    if started.elapsed() >= options.readiness_timeout {
                        return Err(IndependentSpawnError::Readiness {
                            timeout: options.readiness_timeout,
                            reason: "identity verification exceeded launch deadline".into(),
                        });
                    }
                    use std::os::unix::fs::OpenOptionsExt as _;
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .custom_flags(libc::O_NOFOLLOW)
                        .open(acknowledgement.with_file_name("accepted.tmp"))
                        .map_err(classify_io)?
                        .sync_all()
                        .map_err(classify_io)?;
                    if cancelled(options) {
                        return Err(IndependentSpawnError::Cancelled);
                    }
                    if started.elapsed() >= options.readiness_timeout {
                        return Err(IndependentSpawnError::Readiness {
                            timeout: options.readiness_timeout,
                            reason: "acceptance preparation exceeded launch deadline".into(),
                        });
                    }
                    if supervisor.has_exited().map_err(classify_io)? {
                        return Err(IndependentSpawnError::Launch {
                            reason: "independent supervisor exited before launch acceptance".into(),
                        });
                    }
                    // Publication is the acceptance point. No fallible operation
                    // follows it before returning the owned daemon handle.
                    std::fs::rename(
                        acknowledgement.with_file_name("accepted.tmp"),
                        acknowledgement.with_file_name("accepted"),
                    )
                    .map_err(classify_io)?;
                    return Ok(DaemonChild::from_external(
                        pid,
                        Box::new(SystemdChild {
                            identity,
                            supervisor,
                            status: acknowledgement.with_file_name("ack.status"),
                            exit_status: None,
                        }),
                    ));
                }
                if started.elapsed() >= options.readiness_timeout {
                    return Err(IndependentSpawnError::Readiness {
                        timeout: options.readiness_timeout,
                        reason: "helper did not report target identity".into(),
                    });
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        })();
        let result = if let Err(cause) = result {
            let launcher_cleanup = match launcher.take() {
                Some(launcher) => terminate_independent_scheduler_command(launcher),
                None => Ok(()),
            };
            let cleanup = if broker.is_some() {
                cancel_broker_request(&request_path)
            } else {
                stop_unit(&unit)
            };
            match launcher_cleanup.and(cleanup) {
                Ok(()) => Err(cause),
                Err(error) => Err(IndependentSpawnError::CleanupUnconfirmed {
                    cause: Box::new(cause),
                    resource: unit,
                    reason: error.to_string(),
                }),
            }
        } else {
            result
        };
        // A late external helper may still need the request. Preserve its
        // private recovery artifacts whenever cleanup remains unconfirmed.
        if !matches!(
            &result,
            Err(IndependentSpawnError::CleanupUnconfirmed { .. })
        ) {
            let _ = std::fs::remove_file(&request_path);
        }
        result
    }

    fn cancel_broker_request(request: &std::path::Path) -> io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let marker = request.with_file_name("cancel");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&marker)
        {
            Ok(file) => file.sync_all()?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            if cleanup_acknowledged(read_private_control(&request.with_file_name("cancelled"))?)? {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "broker helper cleanup was not acknowledged",
        ))
    }

    fn submit_broker(
        socket: &std::path::Path,
        request: &std::path::Path,
        options: &IndependentSpawnOptions,
        started: Instant,
        may_have_submitted: &mut bool,
    ) -> Result<(), IndependentSpawnError> {
        use std::io::Read as _;
        let stream = crate::independent_broker_transport::connect(
            socket,
            started,
            options.readiness_timeout,
            options.cancellation.as_ref(),
        )
        .map_err(classify_io)?;
        let peer = crate::independent_broker_transport::peer_pid(&stream).map_err(classify_io)?;
        verify_separate_cgroup(peer)?;
        let remaining = options.readiness_timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() || cancelled(options) {
            return Err(if cancelled(options) {
                IndependentSpawnError::Cancelled
            } else {
                IndependentSpawnError::Readiness {
                    timeout: options.readiness_timeout,
                    reason: "broker submission deadline expired".into(),
                }
            });
        }
        let mut stream = crate::independent_broker_transport::DeadlineStream::new(
            stream,
            started,
            options.readiness_timeout,
            options.cancellation.clone(),
        )
        .map_err(classify_io)?;
        crate::independent_broker_transport::read_greeting(&mut stream).map_err(classify_io)?;
        *may_have_submitted = true;
        crate::independent_broker_transport::write_submission(
            &mut stream,
            request,
            remaining.as_millis().saturating_add(1000).min(3_600_000) as u64,
        )
        .map_err(classify_io)?;
        let mut response = [1];
        stream.read_exact(&mut response).map_err(classify_io)?;
        if response == [1] {
            *may_have_submitted = false;
            return Err(IndependentSpawnError::Launch {
                reason: "external broker rejected launch".into(),
            });
        }
        if response != [0] {
            return Err(IndependentSpawnError::Launch {
                reason: "external broker returned an invalid response".into(),
            });
        }
        Ok(())
    }

    fn helper_path() -> io::Result<std::path::PathBuf> {
        let path = match crate::env_vars::INDEPENDENT_HELPER.path() {
            Some(path) => path,
            None => std::env::current_exe()?.with_file_name("running-process-independent-helper"),
        };
        let path = path.canonicalize()?;
        if !path.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "independent helper is not a file",
            ));
        }
        Ok(path)
    }

    fn read_private_control(path: &std::path::Path) -> io::Result<Option<String>> {
        use std::io::Read as _;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe helper control file",
            ));
        }
        let mut bytes = String::new();
        file.take(129).read_to_string(&mut bytes)?;
        if bytes.len() > 128 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "oversized helper control file",
            ));
        }
        Ok(Some(bytes))
    }

    fn read_acknowledgement(path: &std::path::Path) -> io::Result<Option<(u32, u64, u32, u64)>> {
        let Some(bytes) = read_private_control(path)? else {
            return Ok(None);
        };
        parse_launch_acknowledgement(&bytes)
    }

    fn open_identity(pid: u32, expected_start: u64) -> io::Result<StrictProcessHandle> {
        // Require a launch-bound kernel handle: never fall back to signaling
        // a numeric PID which might have been recycled after verification.
        let identity = StrictProcessHandle::open(pid)?;
        if process_start_key(pid)? != expected_start {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "launched process identity changed",
            ));
        }
        Ok(identity)
    }

    fn open_independent_identity(
        pid: u32,
        expected_start: u64,
    ) -> Result<StrictProcessHandle, IndependentSpawnError> {
        let identity = open_identity(pid, expected_start).map_err(classify_io)?;
        verify_separate_cgroup(pid)?;
        if process_start_key(pid).map_err(classify_io)? != expected_start {
            return Err(IndependentSpawnError::Launch {
                reason: "process changed during containment verification".into(),
            });
        }
        Ok(identity)
    }

    #[derive(Debug)]
    struct SystemdChild {
        identity: StrictProcessHandle,
        supervisor: StrictProcessHandle,
        status: std::path::PathBuf,
        exit_status: Option<i32>,
    }
    impl SystemdChild {
        fn record_exit(&mut self, value: &str) -> io::Result<Option<i32>> {
            let status = value.parse().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid helper exit status")
            })?;
            self.exit_status = Some(status);
            // Remove only known protocol artifacts, never recursively remove
            // a runtime directory. Cached status keeps repeated waits stable.
            if let Some(directory) = self.status.parent() {
                for name in ["request", "ack", "accepted", "accepted.tmp", "ack.status"] {
                    let _ = std::fs::remove_file(directory.join(name));
                }
                let _ = std::fs::remove_dir(directory);
            }
            Ok(Some(status))
        }
    }
    impl DaemonChildControl for SystemdChild {
        fn kill(&mut self) -> io::Result<()> {
            // Leave the supervisor alive to reap and persist the exit status.
            self.identity.kill()
        }
        fn wait(&mut self) -> io::Result<i32> {
            loop {
                if let Some(status) = self.try_wait()? {
                    return Ok(status);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        fn try_wait(&mut self) -> io::Result<Option<i32>> {
            if let Some(status) = self.exit_status {
                return Ok(Some(status));
            }
            match read_private_control(&self.status)? {
                Some(value) => self.record_exit(&value),
                None => {
                    if !self.supervisor.has_exited()? {
                        return Ok(None);
                    }
                    // Supervisor may have published between our first read
                    // and its exit. Re-read after observing the exit event.
                    match read_private_control(&self.status)? {
                        Some(value) => self.record_exit(&value),
                        None => Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "independent supervisor exited without recording target status",
                        )),
                    }
                }
            }
        }
    }
    fn verify_separate_cgroup(pid: u32) -> Result<(), IndependentSpawnError> {
        running_process_platform_internal::verify_process_cgroup_separation(pid)
            .map_err(classify_io)
    }

    #[cfg(test)]
    mod containment_tests {
        #[test]
        fn control_files_reject_symlinks_and_oversized_content() {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt;
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("control");
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            file.write_all(&[b'0'; 129]).unwrap();
            assert_eq!(
                super::read_private_control(&path).unwrap_err().kind(),
                std::io::ErrorKind::InvalidData
            );
            let alias = directory.path().join("alias");
            std::os::unix::fs::symlink(&path, &alias).unwrap();
            assert!(super::read_private_control(&alias).is_err());
        }

        #[test]
        fn unavailable_native_support_keeps_its_error_category() {
            assert!(matches!(
                super::classify_io(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "unavailable"
                )),
                super::IndependentSpawnError::Unsupported { .. }
            ));
            assert!(matches!(
                super::classify_io(std::io::Error::from_raw_os_error(libc::ENOSYS)),
                super::IndependentSpawnError::Unsupported { .. }
            ));
        }
    }
    fn stop_unit(unit: &str) -> io::Result<()> {
        let mut command = Command::new("systemctl");
        command.args(["--user", "stop", unit]);
        let mut manager = spawn_independent_helper_child(&mut command)?;
        let started = Instant::now();
        loop {
            match manager.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(_)) => return Err(io::Error::other("systemd refused service cleanup")),
                Ok(None) if started.elapsed() < Duration::from_secs(2) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                result => {
                    terminate_independent_scheduler_command(manager)?;
                    return match result {
                        Err(error) => Err(error),
                        _ => Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "systemd cleanup acknowledgement timed out",
                        )),
                    };
                }
            }
        }
    }
    fn classify_io(error: io::Error) -> IndependentSpawnError {
        if error.kind() == io::ErrorKind::PermissionDenied {
            IndependentSpawnError::PermissionDenied {
                reason: error.to_string(),
            }
        } else if error.kind() == io::ErrorKind::Unsupported
            || error.raw_os_error() == Some(libc::ENOSYS)
        {
            IndependentSpawnError::Unsupported {
                reason: error.to_string(),
            }
        } else {
            IndependentSpawnError::Launch {
                reason: error.to_string(),
            }
        }
    }
    fn unique_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|time| time.as_nanos())
            .unwrap_or_default()
    }
}

#[cfg(windows)]
#[path = "independent_windows.rs"]
mod platform;
#[cfg(not(any(target_os = "linux", windows)))]
mod platform {
    use super::*;
    pub(super) fn capability() -> IndependentSpawnCapability {
        IndependentSpawnCapability {
            available: false,
            backend: None,
            reason: "this platform has no verified independent-spawn backend".into(),
        }
    }
    pub(super) fn spawn(
        _: &mut Command,
        _: &IndependentSpawnOptions,
    ) -> Result<DaemonChild, IndependentSpawnError> {
        Err(IndependentSpawnError::Unsupported {
            reason: capability().reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_acknowledgement_requires_distinct_complete_identities() {
        assert_eq!(
            parse_launch_acknowledgement("12 34 56 78\n").unwrap(),
            Some((12, 34, 56, 78))
        );
        assert_eq!(parse_launch_acknowledgement("12 34 56").unwrap(), None);
        for text in [
            "12 34 12 34\n",
            "0 34 56 78\n",
            "12 34 0 78\n",
            "12 34 56\n",
            "12 34 56 78 extra\n",
            "+12 34 56 78\n",
            "4294967296 34 56 78\n",
            "12 18446744073709551616 56 78\n",
        ] {
            assert_eq!(
                parse_launch_acknowledgement(text).unwrap_err().kind(),
                std::io::ErrorKind::InvalidData
            );
        }
        assert!(parse_launch_acknowledgement(&"0".repeat(129)).is_err());
    }

    #[test]
    fn invalid_builder_inputs_cannot_be_lost_in_command_getters() {
        let mut requests = vec![
            DaemonSpawnRequest::new(""),
            DaemonSpawnRequest::new("private\0program"),
        ];
        let mut request = DaemonSpawnRequest::new("unused");
        request.arg("private\0argument");
        requests.push(request);
        let mut request = DaemonSpawnRequest::new("unused");
        request.current_dir("private\0directory");
        requests.push(request);
        let mut request = DaemonSpawnRequest::new("unused");
        request.env("KEY", "private\0value").env_clear();
        requests.push(request);
        let mut request = DaemonSpawnRequest::new("unused");
        request.env_remove("private\0key");
        requests.push(request);
        for mut request in requests {
            assert!(request.invalid_input);
            let options = IndependentSpawnOptions {
                mode: SpawnMode::Independent,
                ..Default::default()
            };
            let error = match spawn_daemon_request(&mut request, &options) {
                Err(error) => error,
                Ok(_) => panic!("invalid request must never launch"),
            };
            assert!(matches!(error, IndependentSpawnError::Launch { .. }));
            assert!(!error.to_string().contains("private"));
        }
    }

    #[test]
    fn opaque_commands_are_rejected_before_scheduler_submission() {
        let mut command = Command::new("must-not-launch");
        command.stdout(std::process::Stdio::piped());
        assert!(matches!(
            spawn_daemon_with_mode(&mut command, SpawnMode::Independent),
            Err(IndependentSpawnError::Unsupported { .. })
        ));
    }

    #[test]
    fn structured_request_preserves_literals_and_explicit_environment_clear() {
        let mut request = DaemonSpawnRequest::new("private-program");
        request
            .args(["argument with spaces", ""])
            .env("OLD", "private-value")
            .env_clear()
            .env("NEW", "private-value")
            .env_remove("ABSENT")
            .current_dir("relative-directory");
        assert!(request.clear_environment);
        assert_eq!(
            request.command.get_args().collect::<Vec<_>>(),
            vec![OsStr::new("argument with spaces"), OsStr::new("")]
        );
        assert_eq!(
            request.command.get_current_dir(),
            Some(Path::new("relative-directory"))
        );
        let environment = request.command.get_envs().collect::<Vec<_>>();
        assert!(!environment.iter().any(|(key, _)| *key == OsStr::new("OLD")));
        assert!(environment.contains(&(OsStr::new("NEW"), Some(OsStr::new("private-value")))));
        let debug = format!("{request:?}");
        for secret in ["private-program", "private-value", "argument with spaces"] {
            assert!(!debug.contains(secret));
        }
    }

    #[test]
    fn structured_cancellation_precedes_any_launch() {
        let cancellation = IndependentSpawnCancellation::new();
        cancellation.cancel();
        let options = IndependentSpawnOptions {
            mode: SpawnMode::Independent,
            cancellation: Some(cancellation),
            ..Default::default()
        };
        assert!(matches!(
            spawn_daemon_request(&mut DaemonSpawnRequest::new("must-not-launch"), &options),
            Err(IndependentSpawnError::Cancelled)
        ));
    }

    #[test]
    fn cleanup_requires_an_empty_acknowledgement() {
        assert!(!cleanup_acknowledged(None).unwrap());
        assert!(cleanup_acknowledged(Some(String::new())).unwrap());
        for invalid in ["ok", "0", "\n", "cancelled"] {
            assert_eq!(
                cleanup_acknowledged(Some(invalid.into()))
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn inherited_is_default_and_is_the_only_implicit_policy() {
        assert_eq!(
            IndependentSpawnOptions::default().mode,
            SpawnMode::Inherited
        );
    }

    #[test]
    fn cancellation_is_shared_and_observable_before_launch() {
        let cancellation = IndependentSpawnCancellation::new();
        let options = IndependentSpawnOptions {
            mode: SpawnMode::Independent,
            cancellation: Some(cancellation.clone()),
            ..Default::default()
        };
        cancellation.cancel();
        assert!(cancelled(&options));
    }

    #[test]
    fn unsupported_is_distinct_from_permission_and_readiness_failures() {
        let unsupported = IndependentSpawnError::Unsupported {
            reason: "no manager".into(),
        };
        let denied = IndependentSpawnError::PermissionDenied {
            reason: "denied".into(),
        };
        let readiness = IndependentSpawnError::Readiness {
            timeout: Duration::from_secs(1),
            reason: "not ready".into(),
        };
        assert_ne!(unsupported.to_string(), denied.to_string());
        assert_ne!(denied.to_string(), readiness.to_string());
    }

    #[test]
    fn unconfirmed_cleanup_preserves_original_failure() {
        let error = IndependentSpawnError::CleanupUnconfirmed {
            cause: Box::new(IndependentSpawnError::Cancelled),
            resource: "opaque.service".into(),
            reason: "manager timed out".into(),
        };
        assert!(std::error::Error::source(&error)
            .unwrap()
            .to_string()
            .contains("cancelled"));
        assert!(error.to_string().contains("opaque.service"));
    }
}
