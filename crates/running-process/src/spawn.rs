//! Two-mode process spawning. Free functions only — no module-internal traits.
//!
//! Modes (only two; the dangerous combination `detached + caller-pipes` has no
//! API surface):
//!
//!   * [`spawn_daemon`] — detached lifetime, sanitized file-or-NUL stdio,
//!     sanitized handle list, no console window, ignores parent's Ctrl-C. The
//!     returned [`DaemonChild`] does NOT die when dropped.
//!   * [`spawn`] — contained lifetime, caller-controlled stdio via
//!     [`SpawnStdio`], sanitized handle list, no console window by default
//!     (opt in via [`SpawnStdio::show_console`]), bounded drain. The returned
//!     [`SpawnedChild`] kills the child on Drop.
//!
//! ## Sanitized handle inheritance
//!
//! Both modes inherit ONLY the three stdio handles we resolve here. On
//! Windows we use `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` to whitelist exactly
//! the resolved handles. On Unix the spawned child runs a `pre_exec` closure
//! that walks `/proc/self/fd` (or `/dev/fd`) and closes every fd > 2.
//!
//! Motivation: when a process tree has a pipe-redirected ancestor (Python
//! `subprocess.Popen(stdout=PIPE)`, IDE language-server hosts, CI runners,
//! etc.), every intermediate `CreateProcessW(bInheritHandles=TRUE)` on
//! Windows — and every `fork`+`exec` of a non-`O_CLOEXEC` fd on Unix —
//! duplicates that orphaned pipe write-end into the new child. The original
//! reader at the top never sees EOF.
//!
//! Issue: <https://github.com/zackees/running-process/issues/110>.

use std::process::Command;

pub use running_process_platform_internal::platform::process::{
    DaemonChild, DaemonStdio, DaemonStdioSource, SpawnStdio, SpawnedChild, StdioSource,
};

/// Selects the base environment used for a newly spawned process.
///
/// Explicit mutations added through [`Command::env`], [`Command::envs`], or
/// [`Command::env_remove`] are applied after the selected base and therefore
/// always win.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EnvironmentPolicy {
    /// Choose from the process lifetime: contained subprocesses inherit,
    /// while detached daemons start from the logged-in user's baseline.
    #[default]
    Auto,
    /// Inherit the spawning process's environment.
    Inherit,
    /// Start from the logged-in user's machine + user environment, discarding
    /// the spawning process's ambient environment except for the documented
    /// Unix locale, time-zone, and temporary-directory allowlist.
    ///
    /// Windows implements this with `CreateEnvironmentBlock`. Unix
    /// reconstructs a clean login environment from the user's identity
    /// (`getpwuid_r` → `USER`/`LOGNAME`/`HOME`/`SHELL`, platform default
    /// `PATH`, carried-over locale/`TZ`/`TMPDIR`), falling back to inheritance
    /// only when the passwd entry cannot be resolved.
    ///
    /// Consumers that need values such as `CARGO_HOME`, `RUSTUP_HOME`,
    /// `SOLDR_*`, credentials, or runner-specific paths must pass them
    /// explicitly on the [`Command`].
    UserBaseline,
    /// Start from an empty environment.
    Clear,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SpawnLifetime {
    Contained,
    Daemon,
}

impl EnvironmentPolicy {
    pub(crate) fn resolve(self, lifetime: SpawnLifetime) -> Self {
        match (self, lifetime) {
            (Self::Auto, SpawnLifetime::Contained) => Self::Inherit,
            (Self::Auto, SpawnLifetime::Daemon) => Self::UserBaseline,
            (explicit, _) => explicit,
        }
    }

    /// Decode the additive wire policy, falling back to the deprecated
    /// `clear_inherited_env` bit for older clients.
    #[cfg(any(feature = "daemon", feature = "client-async", test))]
    pub(crate) fn from_wire(value: i32, legacy_clear: bool) -> Result<Self, &'static str> {
        match value {
            0 => Ok(if legacy_clear {
                Self::Clear
            } else {
                Self::Inherit
            }),
            1 => Ok(Self::Inherit),
            2 => Ok(Self::UserBaseline),
            3 => Ok(Self::Clear),
            _ => Err("unknown environment policy"),
        }
    }

    /// Encode a resolved policy for either daemon or broker-v2 protobufs.
    pub(crate) fn wire_value(self) -> Result<i32, &'static str> {
        match self {
            Self::Inherit => Ok(1),
            Self::UserBaseline => Ok(2),
            Self::Clear => Ok(3),
            Self::Auto => Err("Auto environment policy must be resolved before serialization"),
        }
    }

    /// Compatibility bit written for servers that predate the wire enum.
    /// `UserBaseline` deliberately degrades to `Clear`, never ambient inherit.
    pub(crate) fn legacy_clear_fallback(self) -> Result<bool, &'static str> {
        match self {
            Self::Inherit => Ok(false),
            Self::UserBaseline | Self::Clear => Ok(true),
            Self::Auto => Err("Auto environment policy must be resolved before serialization"),
        }
    }
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Creation policy for [`spawn_tokio`].
///
/// This compatibility entrypoint lets async daemons keep Tokio's pipe and
/// wait APIs while making `running-process` the sole owner of child-creation
/// policy. It defaults to contained, console-less children.
#[cfg(feature = "client-async")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TokioSpawnOptions {
    /// Terminate the child when Tokio's child handle is dropped.
    pub kill_on_drop: bool,
    /// Whether Windows children may inherit or allocate a visible console.
    pub show_console: bool,
    /// Kill this child at the OS level when the spawning process dies.
    ///
    /// - **Linux**: installs `PR_SET_PDEATHSIG(SIGTERM)` in the child.
    /// - **Windows**: assigns the child to a process-wide `KILL_ON_JOB_CLOSE`
    ///   Job Object, so the child (and its descendants) die when the spawner's
    ///   handle to the job closes — i.e. when the spawner process exits.
    /// - **macOS**: forks a kqueue supervisor before exec and waits for its
    ///   owner/child watches to be registered before reporting spawn success.
    ///
    /// `kill_on_drop` only fires if the spawner runs its `Drop`; this option
    /// covers the crash / SIGKILL / taskkill case where `Drop` never runs. Use
    /// for transient children of a long-lived process (e.g. a daemon's compiler
    /// subprocesses) that must not outlive their owner.
    pub kill_when_owner_dies: bool,
}

#[cfg(feature = "client-async")]
impl Default for TokioSpawnOptions {
    fn default() -> Self {
        Self {
            kill_on_drop: true,
            show_console: false,
            kill_when_owner_dies: false,
        }
    }
}

/// Set on every child spawned through the daemon path, so a process can be
/// recognized as a *declared daemon* rather than inferred to be one.
///
/// # Why a positive marker
///
/// Reapers previously had to infer daemon-ness from the **absence** of
/// [`crate::ORIGINATOR_ENV_VAR`], which `spawn_daemon` strips. But absence is
/// overloaded: it means both "this process deliberately detached itself" and
/// "something in the chain clobbered the environment" — and those are
/// byte-identical at the observation point, so no amount of process-lineage
/// tracking can separate them. See zackees/clud#522, where an
/// ancestry-fallback proposal and a daemon exemption read the same signal and
/// drew opposite conclusions.
///
/// A positive declaration removes the ambiguity: only a process that actually
/// went through the daemon path carries this.
///
/// # Caveat
///
/// This is still an environment variable, so a chain that strips
/// `RUNNING_PROCESS_ORIGINATOR` strips this too. It narrows the ambiguous case
/// rather than eliminating it; a durable answer would need the daemon's
/// supervisor to register the PID somewhere the reaper can read.
///
/// Distinct from `RUNNING_PROCESS_DAEMON_SCOPE`, which names a broker scope
/// and is unrelated.
pub const DAEMON_MARKER_ENV_VAR: &str = "RUNNING_PROCESS_IS_DAEMON";

/// Spawn `command` as a detached daemon. NUL stdio, sanitized handles,
/// no console window, ignores parent's Ctrl-C / SIGINT (Windows:
/// `CREATE_NEW_PROCESS_GROUP` + `DETACHED_PROCESS`; Unix: `setsid` puts the
/// daemon in a new session so it's not in the parent's foreground group).
///
/// Use [`spawn_daemon_with_stdio`] when the daemon must write to stable
/// caller-owned files. Parent stdio and anonymous pipes remain unavailable
/// for detached children.
pub fn spawn_daemon(command: &mut Command) -> std::io::Result<DaemonChild> {
    spawn_daemon_inner(
        command,
        DaemonStdio::default(),
        EnvironmentPolicy::Auto,
        false,
    )
}

/// Spawn a detached daemon with file-or-NUL stdout and stderr.
///
/// Stdin remains connected to null. The supplied handles are duplicated into
/// the sanitized child handle list, so the caller can close its files after
/// this function returns without affecting the daemon.
pub fn spawn_daemon_with_stdio(
    command: &mut Command,
    stdio: DaemonStdio<'_>,
) -> std::io::Result<DaemonChild> {
    spawn_daemon_with_stdio_and_env_policy(command, stdio, EnvironmentPolicy::Auto)
}

/// [`spawn_daemon_with_stdio`] with an explicit environment policy.
pub fn spawn_daemon_with_stdio_and_env_policy(
    command: &mut Command,
    stdio: DaemonStdio<'_>,
    policy: EnvironmentPolicy,
) -> std::io::Result<DaemonChild> {
    spawn_daemon_inner(command, stdio, policy, false)
}

/// Like [`spawn_daemon`] but with explicit control over whether the
/// daemon's inherited env is passed through to the child.
///
/// `clear_env = false` uses [`EnvironmentPolicy::Auto`], matching
/// [`spawn_daemon`].
///
/// `clear_env = true`: child sees ONLY the explicit `command.env(...)`
/// entries. Mirrors `command.env_clear()` semantics for callers using
/// the manual `CreateProcessW` path (Rust stdlib's `env_clear` flag
/// isn't observable through `Command::get_envs`, so our sanitized
/// spawn machinery can't otherwise honour it).
pub fn spawn_daemon_with_clear_env(
    command: &mut Command,
    clear_env: bool,
) -> std::io::Result<DaemonChild> {
    let policy = if clear_env {
        EnvironmentPolicy::Clear
    } else {
        EnvironmentPolicy::Auto
    };
    spawn_daemon_inner(command, DaemonStdio::default(), policy, false)
}

/// Spawn a detached daemon using an explicit environment policy.
///
/// [`EnvironmentPolicy::Auto`] resolves to
/// [`EnvironmentPolicy::UserBaseline`] for daemons, excluding unlisted
/// ambient variables. Use [`EnvironmentPolicy::Inherit`] as the explicit
/// escape hatch for trusted callers that require the full parent environment.
/// In every mode, explicit command environment additions, overrides, and
/// removals are applied last.
pub fn spawn_daemon_with_env_policy(
    command: &mut Command,
    policy: EnvironmentPolicy,
) -> std::io::Result<DaemonChild> {
    spawn_daemon_inner(command, DaemonStdio::default(), policy, false)
}

/// Like [`spawn_daemon`], but the child also **breaks away from any Job
/// Object the spawner belongs to** (Windows; a no-op elsewhere).
///
/// Use this for a daemon that must outlive the process tree that happened to
/// start it — a build cache server, a language server, anything discovered
/// and reused by later, unrelated invocations.
///
/// # Why this is separate from [`spawn_daemon`]
///
/// "Detached lifetime" and "escapes my caller's containment" are different
/// properties, and callers genuinely want them independently. Job Object
/// membership is inherited by every descendant at any depth, and jobs created
/// by this crate carry `KILL_ON_JOB_CLOSE` — so without breakaway the kernel
/// terminates such a daemon the moment the spawner's job handle drops, no
/// matter how detached the daemon made itself.
///
/// But making that unconditional breaks the opposite use: a child spawned as
/// a daemon purely to obtain a sanitized handle list must stay inside the
/// caller's job. `testbins/src/bin/spawner.rs` does exactly this, and
/// `containment_test::test_contained_group_kills_grandchildren` fails if its
/// sleepers escape.
///
/// # Refusal is not silent
///
/// `CREATE_BREAKAWAY_FROM_JOB` is *refused*, not ignored, when the spawner
/// sits inside a job that lacks `JOB_OBJECT_LIMIT_BREAKAWAY_OK`:
/// `CreateProcessW` fails with `ERROR_ACCESS_DENIED`. Outer jobs we do not
/// control are common (CI runners, container supervisors, debuggers), so the
/// spawn retries once with the flag cleared — a daemon that stays contained
/// beats a daemon that fails to start.
pub fn spawn_daemon_breaking_away_from_job(command: &mut Command) -> std::io::Result<DaemonChild> {
    spawn_daemon_inner(
        command,
        DaemonStdio::default(),
        EnvironmentPolicy::Auto,
        true,
    )
}

/// [`spawn_daemon_breaking_away_from_job`] with an explicit env policy.
pub fn spawn_daemon_breaking_away_with_env_policy(
    command: &mut Command,
    policy: EnvironmentPolicy,
) -> std::io::Result<DaemonChild> {
    spawn_daemon_inner(command, DaemonStdio::default(), policy, true)
}

/// Apply the daemon self-declaration to `command`. Split out from
/// [`spawn_daemon_inner`] so the policy is unit-testable without spawning a
/// real detached process.
pub(crate) fn mark_as_daemon(command: &mut Command) {
    command.env(DAEMON_MARKER_ENV_VAR, "1");
}

fn prepare_sync_environment(
    policy: EnvironmentPolicy,
) -> std::io::Result<running_process_platform_internal::platform::process::SyncEnvironment> {
    use running_process_platform_internal::platform::process::SyncEnvironment;

    if policy == EnvironmentPolicy::Inherit {
        return Ok(SyncEnvironment::Inherit);
    }
    if policy == EnvironmentPolicy::Auto {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Auto environment policy must be resolved before platform spawn",
        ));
    }

    let baseline = match policy {
        EnvironmentPolicy::UserBaseline => crate::environment::user_baseline_environment()?,
        EnvironmentPolicy::Clear => Vec::new(),
        EnvironmentPolicy::Auto | EnvironmentPolicy::Inherit => unreachable!(),
    };
    Ok(SyncEnvironment::Explicit(baseline))
}

fn spawn_daemon_inner(
    command: &mut Command,
    stdio: DaemonStdio<'_>,
    policy: EnvironmentPolicy,
    breakaway: bool,
) -> std::io::Result<DaemonChild> {
    // Every daemon-spawn variant funnels through here, so this is the one
    // place that can mark them all — including the free functions consumers
    // like zccache call directly.
    mark_as_daemon(command);
    let policy = policy.resolve(SpawnLifetime::Daemon);
    let environment = prepare_sync_environment(policy)?;
    running_process_platform_internal::platform::process::spawn_sync_daemon(
        command,
        stdio,
        environment,
        breakaway,
    )
}

/// Spawn `command` as a contained child with caller-controlled stdio.
/// Sanitized handles, and no console (`DETACHED_PROCESS` on Windows). Child
/// dies when the returned
/// [`SpawnedChild`] is dropped.
pub fn spawn(command: &mut Command, stdio: SpawnStdio<'_>) -> std::io::Result<SpawnedChild> {
    spawn_with_env_policy(command, stdio, EnvironmentPolicy::Auto)
}

/// Spawn a contained child using an explicit environment policy.
pub fn spawn_with_env_policy(
    command: &mut Command,
    stdio: SpawnStdio<'_>,
    policy: EnvironmentPolicy,
) -> std::io::Result<SpawnedChild> {
    let policy = policy.resolve(SpawnLifetime::Contained);
    let environment = prepare_sync_environment(policy)?;
    running_process_platform_internal::platform::process::spawn_sync(command, stdio, environment)
}

/// Spawn a Tokio child through the centralized process-creation boundary.
///
/// Callers retain Tokio's async stdin/stdout/stderr and wait APIs, but may not
/// apply platform creation flags themselves. On Windows, console suppression
/// is owned here. Use [`spawn`] when the stronger sanitized-handle-list and
/// kill-on-close Job Object contract is required.
#[cfg(feature = "client-async")]
pub fn spawn_tokio(
    command: &mut tokio::process::Command,
    options: TokioSpawnOptions,
) -> std::io::Result<tokio::process::Child> {
    command.kill_on_drop(options.kill_on_drop);
    running_process_platform_internal::configure_compat_tokio_command(
        command,
        options.show_console,
        options.kill_when_owner_dies,
    )?;

    let child = command.spawn()?;

    // A containment failure is reported, not swallowed. `kill_when_owner_dies`
    // is asked for by callers that must not leak children -- zccache's compile
    // workers are the case this exists for -- and a spawn that quietly returns
    // an uncontained child hands them exactly the orphan they asked to avoid.
    running_process_platform_internal::after_compat_tokio_spawn(
        &child,
        options.kill_when_owner_dies,
    )?;

    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    use std::time::Duration;

    fn assert_child_auto_traits<T>()
    where
        T: Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe,
    {
    }

    #[test]
    fn child_handles_preserve_thread_and_unwind_auto_traits() {
        assert_child_auto_traits::<DaemonChild>();
        assert_child_auto_traits::<SpawnedChild>();
    }

    #[derive(Clone, PartialEq, Message)]
    struct LegacyClearAtTag4 {
        #[prost(bool, tag = "4")]
        clear_inherited_env: bool,
    }

    #[derive(Clone, PartialEq, Message)]
    struct LegacyClearAtTag5 {
        #[prost(bool, tag = "5")]
        clear_inherited_env: bool,
    }

    #[cfg(feature = "client-async")]
    #[test]
    fn kill_when_owner_dies_defaults_off() {
        // Opt-in only — existing callers keep today's behavior.
        assert!(!TokioSpawnOptions::default().kill_when_owner_dies);
    }

    #[test]
    fn spawn_stdio_default_has_sane_values() {
        let s = SpawnStdio::default();
        assert!(matches!(s.stdin, StdioSource::Null));
        assert!(matches!(s.stdout, StdioSource::Parent));
        assert!(matches!(s.stderr, StdioSource::Parent));
        assert_eq!(s.drain_timeout, Some(Duration::from_secs(2)));
        // No console window by default — opt-in only.
        assert!(!s.show_console);
    }

    #[test]
    fn daemon_stdio_default_is_null() {
        let stdio = DaemonStdio::default();
        assert!(matches!(stdio.stdout, DaemonStdioSource::Null));
        assert!(matches!(stdio.stderr, DaemonStdioSource::Null));
    }

    #[test]
    fn auto_environment_policy_depends_on_lifetime() {
        assert_eq!(
            EnvironmentPolicy::Auto.resolve(SpawnLifetime::Contained),
            EnvironmentPolicy::Inherit
        );
        assert_eq!(
            EnvironmentPolicy::Auto.resolve(SpawnLifetime::Daemon),
            EnvironmentPolicy::UserBaseline
        );
    }

    #[test]
    fn explicit_environment_policy_is_not_rewritten() {
        for policy in [
            EnvironmentPolicy::Inherit,
            EnvironmentPolicy::UserBaseline,
            EnvironmentPolicy::Clear,
        ] {
            assert_eq!(policy.resolve(SpawnLifetime::Contained), policy);
            assert_eq!(policy.resolve(SpawnLifetime::Daemon), policy);
        }
    }

    #[test]
    fn wire_environment_policy_preserves_legacy_and_fails_closed() {
        assert_eq!(
            EnvironmentPolicy::from_wire(0, false),
            Ok(EnvironmentPolicy::Inherit)
        );
        assert_eq!(
            EnvironmentPolicy::from_wire(0, true),
            Ok(EnvironmentPolicy::Clear)
        );
        assert_eq!(
            EnvironmentPolicy::from_wire(1, true),
            Ok(EnvironmentPolicy::Inherit)
        );
        assert_eq!(
            EnvironmentPolicy::from_wire(2, false),
            Ok(EnvironmentPolicy::UserBaseline)
        );
        assert_eq!(
            EnvironmentPolicy::from_wire(3, false),
            Ok(EnvironmentPolicy::Clear)
        );
        assert!(EnvironmentPolicy::from_wire(99, false).is_err());
        assert_eq!(
            EnvironmentPolicy::UserBaseline.legacy_clear_fallback(),
            Ok(true)
        );
        assert!(EnvironmentPolicy::Auto.wire_value().is_err());
    }

    #[test]
    fn old_clients_and_new_servers_interoperate_on_all_spawn_messages() {
        use crate::broker::protocol_v2::SessionStart;
        use crate::proto::daemon::{
            SpawnDaemonRequest, SpawnPipeSessionRequest, SpawnPtySessionRequest,
        };

        for legacy_clear in [false, true] {
            let tag5 = LegacyClearAtTag5 {
                clear_inherited_env: legacy_clear,
            }
            .encode_to_vec();
            let daemon = SpawnDaemonRequest::decode(tag5.as_slice()).unwrap();
            let session = SessionStart::decode(tag5.as_slice()).unwrap();
            let expected = if legacy_clear {
                EnvironmentPolicy::Clear
            } else {
                EnvironmentPolicy::Inherit
            };
            assert_eq!(
                EnvironmentPolicy::from_wire(daemon.environment_policy, daemon.clear_inherited_env),
                Ok(expected)
            );
            assert_eq!(
                EnvironmentPolicy::from_wire(
                    session.environment_policy,
                    session.clear_inherited_env
                ),
                Ok(expected)
            );

            let tag4 = LegacyClearAtTag4 {
                clear_inherited_env: legacy_clear,
            }
            .encode_to_vec();
            let pipe = SpawnPipeSessionRequest::decode(tag4.as_slice()).unwrap();
            let pty = SpawnPtySessionRequest::decode(tag4.as_slice()).unwrap();
            assert_eq!(
                EnvironmentPolicy::from_wire(pipe.environment_policy, pipe.clear_inherited_env),
                Ok(expected)
            );
            assert_eq!(
                EnvironmentPolicy::from_wire(pty.environment_policy, pty.clear_inherited_env),
                Ok(expected)
            );
        }
    }

    #[test]
    fn new_clients_dual_write_fallback_for_old_servers_on_all_spawn_messages() {
        use crate::broker::protocol_v2::SessionStart;
        use crate::proto::daemon::{
            SpawnDaemonRequest, SpawnPipeSessionRequest, SpawnPtySessionRequest,
        };

        for policy in [
            EnvironmentPolicy::Inherit,
            EnvironmentPolicy::UserBaseline,
            EnvironmentPolicy::Clear,
        ] {
            let legacy_clear = policy.legacy_clear_fallback().unwrap();
            let wire_policy = policy.wire_value().unwrap();
            let daemon = SpawnDaemonRequest {
                clear_inherited_env: legacy_clear,
                environment_policy: wire_policy,
                ..Default::default()
            };
            let pipe = SpawnPipeSessionRequest {
                clear_inherited_env: legacy_clear,
                environment_policy: wire_policy,
                ..Default::default()
            };
            let pty = SpawnPtySessionRequest {
                clear_inherited_env: legacy_clear,
                environment_policy: wire_policy,
                ..Default::default()
            };
            let session = SessionStart {
                clear_inherited_env: legacy_clear,
                environment_policy: wire_policy,
                ..Default::default()
            };

            assert_eq!(
                LegacyClearAtTag5::decode(daemon.encode_to_vec().as_slice())
                    .unwrap()
                    .clear_inherited_env,
                legacy_clear
            );
            assert_eq!(
                LegacyClearAtTag4::decode(pipe.encode_to_vec().as_slice())
                    .unwrap()
                    .clear_inherited_env,
                legacy_clear
            );
            assert_eq!(
                LegacyClearAtTag4::decode(pty.encode_to_vec().as_slice())
                    .unwrap()
                    .clear_inherited_env,
                legacy_clear
            );
            assert_eq!(
                LegacyClearAtTag5::decode(session.encode_to_vec().as_slice())
                    .unwrap()
                    .clear_inherited_env,
                legacy_clear
            );
        }
    }

    #[cfg(feature = "client-async")]
    #[test]
    fn tokio_spawn_defaults_to_contained_consoleless_children() {
        assert_eq!(
            TokioSpawnOptions::default(),
            TokioSpawnOptions {
                kill_on_drop: true,
                show_console: false,
                kill_when_owner_dies: false,
            }
        );
    }
}
