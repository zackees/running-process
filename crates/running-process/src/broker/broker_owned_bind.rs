//! Broker-owned bind: the broker binds the endpoint and hands the listener to
//! the daemon it spawns (#500 slice 32, Option B).
//!
//! # What this buys over Option A
//!
//! The existing path ([`CommandBackendLauncher`]) spawns the daemon and then
//! probes until the daemon's own `bind` succeeds. The endpoint is therefore
//! unreachable for however long the daemon takes to reach that call, and a
//! client connecting in that window sees a refusal rather than a queue.
//!
//! Here the broker binds *first*, so the endpoint is listening — and clients
//! queue in the accept backlog — before the daemon's `main` has run at all.
//! That is what
//! `broker_owned_bind_endpoint_is_probe_able_before_daemon_main_runs` asserts.
//!
//! [`CommandBackendLauncher`]: crate::broker::server::backend_launcher::CommandBackendLauncher
//!
//! # Why this is Unix-only, and not for want of trying
//!
//! Handing over a bound listener needs a kernel object whose ownership can
//! move across a spawn. A Unix domain socket is exactly that:
//! `interprocess`'s UDS listener exposes `AsFd` and `From<OwnedFd>`, so the
//! broker passes a descriptor and the child rebuilds the listener from it.
//!
//! A Windows named pipe has no such object. Its "listener" is a single pipe
//! *instance* that **becomes** the connection when a client arrives, after
//! which a fresh instance is created from the pipe name. Duplicating that
//! handle into a child would hand over one half-open instance, not a
//! listener — and the child needs the pipe name regardless, which is what
//! Option A already passes.
//!
//! So Windows keeps Option A and this module reports
//! [`Support::Unsupported`] there, with a reason, rather than pretending. That
//! matches how the rest of this crate handles per-platform gaps (see
//! `ObserverCapabilities` and `IconSupport`): an honest "no, because" beats a
//! silent degradation a caller cannot distinguish from success.

// Only the Unix half hands a listener to a child; on Windows `support()`
// reports the gap and nothing here touches a `Command`.
#[cfg(unix)]
use std::process::Command;

/// Environment variable naming the inherited listener's descriptor.
///
/// A const rather than a literal because the repo's env-literal lint requires
/// it, and because a rename must not be able to leave one spelling behind in
/// the broker while the daemon reads another — the failure mode there is a
/// daemon that silently binds its own socket and a broker that thinks it
/// handed one over.
pub const INHERITED_LISTENER_FD_ENV: &str = "RUNNING_PROCESS_BROKER_LISTENER_FD";

/// Whether this platform can hand a bound listener to a spawned daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Support {
    /// The broker can bind and pass the listener.
    Supported,
    /// It cannot, and this is why.
    Unsupported {
        /// Stable, human-readable reason. Stable so a caller may log it and a
        /// test may assert on it.
        reason: &'static str,
    },
}

impl Support {
    /// Whether broker-owned bind can be used here.
    pub fn is_supported(&self) -> bool {
        matches!(self, Support::Supported)
    }
}

/// Report whether broker-owned bind works on this platform.
///
/// Callers are expected to fall back to the spawn-then-probe path when this
/// is [`Support::Unsupported`]; it is a capability query, not an error.
pub fn support() -> Support {
    #[cfg(unix)]
    {
        Support::Supported
    }
    #[cfg(windows)]
    {
        Support::Unsupported {
            reason: "a Windows named-pipe listener is a single instance that becomes the \
                     connection on accept, so there is no bound listener object to hand to \
                     a child; the spawn-then-probe path applies instead",
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        Support::Unsupported {
            reason: "no listener-passing mechanism is implemented for this platform",
        }
    }
}

/// A listener bound by the broker, ready to be inherited by a child.
#[cfg(unix)]
#[derive(Debug)]
pub struct InheritableListener {
    listener: interprocess::os::unix::uds_local_socket::Listener,
}

#[cfg(unix)]
impl InheritableListener {
    /// Bind `endpoint` in this process.
    ///
    /// The socket is listening the moment this returns, which is the entire
    /// point: a probe issued immediately afterwards succeeds even though no
    /// daemon exists yet.
    pub fn bind(endpoint: &str) -> std::io::Result<Self> {
        use interprocess::local_socket::{GenericFilePath, ListenerOptions, ToFsName as _};
        use interprocess::os::unix::uds_local_socket::Listener as UdsListener;

        let name = endpoint.to_fs_name::<GenericFilePath>()?;
        // Built through the generic options so the socket file's mode and
        // cleanup semantics match every other listener this crate creates;
        // only the concrete type differs, because the generic `Listener` enum
        // does not expose a descriptor.
        let listener: UdsListener = ListenerOptions::new().name(name).create_sync_as()?;
        Ok(Self { listener })
    }

    /// Arrange for `command`'s child to inherit this listener.
    ///
    /// Clears `FD_CLOEXEC` so the descriptor survives `exec`, and records its
    /// number in the environment for the child to find.
    pub fn prepare(&self, command: &mut Command) -> std::io::Result<()> {
        use std::os::fd::AsFd as _;
        let fd = self.listener.as_fd();
        clear_cloexec(&fd)?;
        // The raw number is meaningful only in the child's descriptor table,
        // which is a copy of ours — hence passing the integer rather than
        // anything richer.
        command.env(
            INHERITED_LISTENER_FD_ENV,
            std::os::fd::AsRawFd::as_raw_fd(&fd).to_string(),
        );
        Ok(())
    }
}

/// Clear `FD_CLOEXEC` so the descriptor survives `exec`.
///
/// Rust sets `CLOEXEC` on everything it opens, which is the right default —
/// without it every spawned process inherits whatever happened to be open.
/// Passing a listener deliberately is the exception, so the flag is cleared
/// on exactly the one descriptor being handed over.
#[cfg(unix)]
fn clear_cloexec(fd: &std::os::fd::BorrowedFd<'_>) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let raw = fd.as_raw_fd();
    // SAFETY: `raw` comes from a live BorrowedFd, so it is a valid open
    // descriptor for the duration of these calls.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let cleared = flags & !libc::FD_CLOEXEC;
    // SAFETY: as above; `cleared` is the flag set we just read, minus one bit.
    if unsafe { libc::fcntl(raw, libc::F_SETFD, cleared) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Whether `fd` is open and is a socket in the listening state.
///
/// Used to refuse a descriptor number that names something else before taking
/// ownership of it. `SO_ACCEPTCONN` is the narrowest question that answers
/// "is this the thing the broker bound": a connected socket answers `false`,
/// while a plain file or pipe is rejected by the call itself with `ENOTSOCK`
/// and a closed descriptor with `EBADF`. All three outcomes refuse the
/// handover; only a listening socket is adopted.
#[cfg(unix)]
fn is_listening_socket(fd: i32) -> std::io::Result<bool> {
    let mut listening: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `listening` and `len` are stack locals of exactly the type and
    // size `getsockopt` is told to expect; the call only reads `fd` and writes
    // through those two pointers. An invalid `fd` returns EBADF rather than
    // touching memory.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ACCEPTCONN,
            std::ptr::addr_of_mut!(listening).cast(),
            std::ptr::addr_of_mut!(len),
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(listening != 0)
}

/// Take ownership of `fd` as an inherited listener, or refuse it.
///
/// Split out from [`recover_from_env`] so the refusal path is reachable from a
/// test without setting an environment variable — env-mutating tests race
/// under a parallel runner. Keeping the check here rather than at the call
/// site also means there is exactly one route to `from_raw_fd`.
#[cfg(unix)]
fn adopt_descriptor(fd: i32) -> std::io::Result<crate::broker::brokered_backend::IpcListener> {
    use interprocess::os::unix::uds_local_socket::Listener as UdsListener;
    use std::os::fd::{FromRawFd as _, OwnedFd};

    // Adopting a descriptor means taking ownership of it, and the number
    // arrived as text. An unchecked `from_raw_fd` on a value naming something
    // this process already owns — `1` is the obvious one — would create a
    // second owner of stdout and close it on drop. So the number must name a
    // listening socket before it is adopted; anything else fails closed. This
    // is a soundness guard, not a trust boundary: whoever sets that variable
    // already controls the daemon's execution.
    if !is_listening_socket(fd)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{INHERITED_LISTENER_FD_ENV}={fd} does not name a listening socket"),
        ));
    }
    // SAFETY: `fd` is open and is a listening socket, both just verified via
    // `getsockopt`, and nothing else in this process owns it — it was created
    // in the broker and inherited across `exec` into a fresh descriptor table.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    Ok(UdsListener::from(owned).into())
}

/// Recover a listener the broker bound and passed to this process.
///
/// `Ok(None)` means no listener was passed — an ordinary outcome for a daemon
/// started any other way, and the caller should bind for itself. An `Err`
/// means one was advertised but could not be adopted, which is worth
/// surfacing rather than silently falling back: binding a second listener at
/// the same endpoint would leave the broker holding one nobody serves.
#[cfg(unix)]
pub fn recover_from_env() -> std::io::Result<Option<crate::broker::brokered_backend::IpcListener>> {
    let Some(raw) = std::env::var_os(INHERITED_LISTENER_FD_ENV) else {
        return Ok(None);
    };
    let raw = raw.to_string_lossy();
    let fd: i32 = raw.trim().parse().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{INHERITED_LISTENER_FD_ENV}={raw:?} is not a descriptor number"),
        )
    })?;
    if fd < 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{INHERITED_LISTENER_FD_ENV}={fd} is not a valid descriptor"),
        ));
    }
    adopt_descriptor(fd).map(Some)
}

/// Windows has no listener to recover; see the module docs.
#[cfg(not(unix))]
pub fn recover_from_env() -> std::io::Result<Option<crate::broker::brokered_backend::IpcListener>> {
    Ok(None)
}

#[cfg(test)]
mod tests;
