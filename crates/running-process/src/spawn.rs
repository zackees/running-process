//! Two-mode process spawning. Free functions only — no module-internal traits.
//!
//! Modes (only two; the dangerous combination `detached + caller-pipes` has no
//! API surface):
//!
//!   * [`spawn_daemon`] — detached lifetime, NUL stdio, sanitized handle list,
//!     no console window, ignores parent's Ctrl-C. The returned [`DaemonChild`]
//!     does NOT die when dropped.
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

#[cfg(unix)]
use std::os::fd::BorrowedFd;
#[cfg(windows)]
use std::os::windows::io::BorrowedHandle;
use std::process::Command;
use std::time::Duration;

/// Selects the base environment used for a newly spawned process.
///
/// Explicit values added through [`Command::env`] or [`Command::envs`]
/// are applied after the selected base and therefore win on duplicate keys.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EnvironmentPolicy {
    /// Choose from the process lifetime: contained subprocesses inherit,
    /// while detached daemons start from the logged-in user's baseline.
    #[default]
    Auto,
    /// Inherit the spawning process's environment.
    Inherit,
    /// Start from the logged-in user's machine + user environment.
    ///
    /// Windows implements this with `CreateEnvironmentBlock`. Unix
    /// reconstructs a clean login environment from the user's identity
    /// (`getpwuid_r` → `USER`/`LOGNAME`/`HOME`/`SHELL`, platform default
    /// `PATH`, carried-over locale/`TZ`/`TMPDIR`), falling back to
    /// inheritance only when the passwd entry cannot be resolved.
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
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Caller-supplied stdio bindings for [`spawn`].
///
/// Each of `stdin`, `stdout`, `stderr` is independently a [`StdioSource`].
/// `drain_timeout` bounds the post-mortem wait the watcher thread applies
/// before force-closing any wrapper-held pipe ends so the parent observes
/// EOF after the child exits. `None` means the wrapper never auto-closes;
/// the parent is responsible for closing the pipes when it's done reading.
///
/// `show_console` (Windows-only effect) controls whether the child gets a
/// console window. Default is `false` — `CREATE_NO_WINDOW` is set, so the
/// child has no console regardless of how the parent was launched. Set this
/// to `true` only when you actually want the child to inherit / allocate a
/// console (interactive subprocess that should be visible to the user).
pub struct SpawnStdio<'a> {
    /// Source connected to the child's standard input.
    pub stdin: StdioSource<'a>,
    /// Source connected to the child's standard output.
    pub stdout: StdioSource<'a>,
    /// Source connected to the child's standard error.
    pub stderr: StdioSource<'a>,
    /// Maximum time the watcher waits before closing wrapper-held pipe ends.
    pub drain_timeout: Option<Duration>,
    /// Whether Windows children may inherit or allocate a visible console.
    pub show_console: bool,
}

impl Default for SpawnStdio<'_> {
    fn default() -> Self {
        Self {
            stdin: StdioSource::Null,
            stdout: StdioSource::Parent,
            stderr: StdioSource::Parent,
            drain_timeout: Some(Duration::from_secs(2)),
            show_console: false,
        }
    }
}

/// Per-slot source describing what the child should inherit for one of
/// stdin / stdout / stderr.
pub enum StdioSource<'a> {
    /// Connect this slot to the platform null device (`NUL` / `/dev/null`).
    Null,
    /// Inherit the parent's corresponding standard handle. The kernel
    /// receives a fresh inheritable duplicate; the parent's original slot
    /// is untouched.
    Parent,
    /// Bind this slot to a caller-owned OS handle. The wrapper duplicates
    /// the handle into an inheritable copy for the child; the caller
    /// retains its own handle and is responsible for closing it.
    #[cfg(windows)]
    Handle(BorrowedHandle<'a>),
    /// Bind this slot to a caller-owned file descriptor. Equivalent to
    /// `StdioSource::Handle` on Unix.
    #[cfg(unix)]
    Fd(BorrowedFd<'a>),
    /// Create a fresh anonymous pipe. The child gets one end; the parent
    /// gets the other via [`SpawnedChild`]'s `stdin` / `stdout` / `stderr`
    /// fields.
    Pipe,
    #[doc(hidden)]
    _Phantom(std::marker::PhantomData<&'a ()>),
}

// _Phantom is uninhabitable from outside: PhantomData<&'a ()> is a private
// constructor in practice (the variant is doc(hidden) and not constructed
// anywhere in this crate). It's only here so the `'a` lifetime is always
// used regardless of which cfg branch is active.

/// Handle to a detached daemon spawned via [`spawn_daemon`].
///
/// The daemon child always has stdin/stdout/stderr connected to the
/// platform null device (`NUL` on Windows, `/dev/null` on Unix) — a
/// detached process with inherited stdio is the classic crash-on-first-
/// `println!` failure mode after the parent closes its end, so the
/// daemon-spawn path forecloses that by construction. Dropping
/// `DaemonChild` does NOT terminate the daemon; it only closes the OS
/// handle the wrapper held. Call [`DaemonChild::kill`] to terminate.
pub struct DaemonChild {
    pid: u32,
    #[cfg(windows)]
    handle: imp::OwnedHandle,
    #[cfg(unix)]
    child: std::process::Child,
}

impl DaemonChild {
    /// Process ID.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Forcibly terminate the child. Best-effort.
    pub fn kill(&mut self) -> std::io::Result<()> {
        #[cfg(windows)]
        {
            imp::terminate(&self.handle)
        }
        #[cfg(unix)]
        {
            self.child.kill()
        }
    }

    /// Block until the child exits and return its exit code.
    pub fn wait(&mut self) -> std::io::Result<i32> {
        #[cfg(windows)]
        {
            imp::wait(&self.handle)
        }
        #[cfg(unix)]
        {
            let status = self.child.wait()?;
            Ok(unix_exit_code(status))
        }
    }

    /// Non-blocking variant of [`Self::wait`].
    pub fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        #[cfg(windows)]
        {
            imp::try_wait(&self.handle)
        }
        #[cfg(unix)]
        {
            Ok(self.child.try_wait()?.map(unix_exit_code))
        }
    }
}

/// Handle to a contained child spawned via [`spawn`].
///
/// On Drop, `SpawnedChild` synchronously kills the child:
///   * Windows: closes the Job Object handle; `KILL_ON_JOB_CLOSE` causes the
///     kernel to terminate every process in the job (the child and its
///     descendants).
///   * Unix: `killpg(pgid, SIGKILL)` and `waitpid` to reap.
///
/// The optional `stdin` / `stdout` / `stderr` fields are present when the
/// corresponding [`StdioSource`] was [`StdioSource::Pipe`]; otherwise they
/// are `None`.
pub struct SpawnedChild {
    /// Parent-side pipe for writing to child stdin when requested.
    pub stdin: Option<std::process::ChildStdin>,
    /// Parent-side pipe for reading child stdout when requested.
    pub stdout: Option<std::process::ChildStdout>,
    /// Parent-side pipe for reading child stderr when requested.
    pub stderr: Option<std::process::ChildStderr>,
    pid: u32,
    #[cfg(windows)]
    inner: imp::SpawnedInner,
    #[cfg(unix)]
    inner: unix_impl::SpawnedInner,
}

impl SpawnedChild {
    /// Process ID of the spawned child.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Forcibly terminate the child. Best-effort.
    pub fn kill(&mut self) -> std::io::Result<()> {
        #[cfg(windows)]
        {
            self.inner.kill()
        }
        #[cfg(unix)]
        {
            self.inner.kill()
        }
    }

    /// Block until the child exits and return its exit code.
    pub fn wait(&mut self) -> std::io::Result<i32> {
        #[cfg(windows)]
        {
            self.inner.wait()
        }
        #[cfg(unix)]
        {
            self.inner.wait()
        }
    }

    /// Non-blocking variant of [`Self::wait`].
    pub fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        #[cfg(windows)]
        {
            self.inner.try_wait()
        }
        #[cfg(unix)]
        {
            self.inner.try_wait()
        }
    }
}

impl Drop for SpawnedChild {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            self.inner.shutdown();
        }
        #[cfg(unix)]
        {
            self.inner.shutdown();
        }
    }
}

/// Spawn `command` as a detached daemon. NUL stdio, sanitized handles,
/// no console window, ignores parent's Ctrl-C / SIGINT (Windows:
/// `CREATE_NEW_PROCESS_GROUP` + `DETACHED_PROCESS`; Unix: `setsid` puts the
/// daemon in a new session so it's not in the parent's foreground group).
///
/// The NUL-stdio guarantee is enforced internally by the platform impls
/// and is not configurable — a detached daemon needs sunk stdio to
/// avoid crashing on later `println!`/`eprintln!` after the parent
/// closes its handles.
pub fn spawn_daemon(command: &mut Command) -> std::io::Result<DaemonChild> {
    spawn_daemon_with_env_policy(command, EnvironmentPolicy::Auto)
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
    spawn_daemon_with_env_policy(command, policy)
}

/// Spawn a detached daemon using an explicit environment policy.
pub fn spawn_daemon_with_env_policy(
    command: &mut Command,
    policy: EnvironmentPolicy,
) -> std::io::Result<DaemonChild> {
    let policy = policy.resolve(SpawnLifetime::Daemon);
    #[cfg(windows)]
    {
        imp::spawn_daemon(command, policy)
    }
    #[cfg(unix)]
    {
        unix_impl::spawn_daemon(command, policy)
    }
}

/// Spawn `command` as a contained child with caller-controlled stdio.
/// Sanitized handles, CREATE_NO_WINDOW. Child dies when the returned
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
    #[cfg(windows)]
    {
        imp::spawn(command, stdio, policy)
    }
    #[cfg(unix)]
    {
        unix_impl::spawn(command, stdio, policy)
    }
}

#[cfg(unix)]
fn unix_exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| -status.signal().unwrap_or(1))
}

// ── Windows implementation ──────────────────────────────────────────────────

#[cfg(windows)]
#[path = "spawn_imp_windows.rs"]
mod imp;

#[cfg(unix)]
#[path = "spawn_imp_unix.rs"]
mod unix_impl;
#[cfg(test)]
mod tests {
    use super::*;

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
}
