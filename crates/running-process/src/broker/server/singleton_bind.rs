//! Reusable per-user-session singleton bind for a v2 local-socket listener.
//!
//! Extracted from `running-process-broker-v2`'s `main()` (soldr#2361 Phase 2
//! prep) so any consumer that wants to run v2 broker-server logic — the
//! scaffold binary in this crate, or soldr's own embedded broker role —
//! shares one tested implementation of "bind a name exactly once per user
//! session, refuse a second bind instead of racing it" rather than each
//! reimplementing the bind/singleton/stale-socket-cleanup dance.
//!
//! The stale-socket-cleanup subtlety here is load-bearing: see
//! [`bind_singleton`]'s docs and running-process#899 for the concurrency bug
//! this module's shape specifically avoids.

use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::platform::ipc::Listener;

const STALE_RECOVERY_LOCK_TIMEOUT: Duration = Duration::from_secs(1);
const STALE_RECOVERY_LOCK_POLL: Duration = Duration::from_millis(10);

/// Resolve the bare pipe/socket name into a full, platform-specific bind
/// path: `\\.\pipe\<bare_name>` on Windows, or a file under a per-user
/// runtime directory on Unix (macOS additionally hashes the leaf to fit
/// `sun_path`'s 104-byte limit).
pub fn resolve_socket_path(bare_name: &str) -> Result<String, String> {
    crate::platform::ipc::broker_endpoint_name(bare_name, false).map_err(|error| error.to_string())
}

/// Resolve an install-path-scoped broker name without adding user identity.
///
/// Unlike [`resolve_socket_path`], this endpoint must remain identical across
/// users and runtime environments: a user-local install is already unique by
/// its canonical path hash, while one machine-wide install intentionally has
/// one machine-wide endpoint. Windows named pipes are already machine-global.
/// Unix uses the machine-global temporary root and a compact hash to stay
/// within every platform's `sun_path` limit.
pub fn resolve_path_scoped_socket_path(bare_name: &str) -> Result<String, String> {
    crate::platform::ipc::broker_endpoint_name(bare_name, true).map_err(|error| error.to_string())
}

/// Classify a [`Listener::bind`] error as "another process is already bound at
/// this name" vs any other bind failure.
///
/// `AddrInUse` / `WouldBlock` are the canonical "another listener already
/// owns this name" signals on Unix-style transports. **Windows named-pipe
/// bind reports the same condition as `PermissionDenied`**
/// (ERROR_ACCESS_DENIED, raw os error 5) because the existing pipe
/// instance's ACL blocks the second bind. Treat that case as already-bound
/// too — a "true" permission problem on a per-user runtime-dir socket path
/// is extremely rare in practice (the path lives under `XDG_RUNTIME_DIR` /
/// `TMPDIR`, always writable by the current user).
pub fn is_already_bound_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::AddrInUse | io::ErrorKind::WouldBlock | io::ErrorKind::PermissionDenied,
    )
}

/// Source-compatible stale-endpoint helper for filesystem-backed transports.
/// Tells a genuinely orphaned socket path (left behind by a process that
/// exited without cleaning up) apart from a path where a live
/// peer is listening right now — the two look identical to `bind`
/// (`AddrInUse` either way). A connect probe distinguishes them: nothing is
/// listening if the connect itself fails to even reach a peer
/// (`ConnectionRefused` — the classic "orphaned socket file, no listener"
/// signal — or `NotFound`); any other outcome, including a successful
/// connect, means treat the path as live and leave it alone. Non-filesystem
/// transports return `false` because they leave no endpoint file to retire.
pub fn unix_socket_path_is_stale(socket_path: &str) -> bool {
    crate::platform::ipc::Endpoint::new(socket_path.to_owned())
        .map(|endpoint| endpoint.is_stale())
        .unwrap_or(false)
}

/// Build an `interprocess` [`Name`](interprocess::local_socket::Name) from a
/// resolved socket path (see [`resolve_socket_path`]).
pub fn wrap_socket_name(socket_path: &str) -> Result<interprocess::local_socket::Name<'_>, String> {
    running_process_platform_internal::legacy_ipc_name(socket_path)
}

/// Why [`bind_singleton`] refused to bind.
#[derive(Debug)]
pub enum BindSingletonError {
    /// Resolving `socket_path` into a platform endpoint failed.
    InvalidName(String),
    /// Another process already holds this name — the singleton refusal
    /// path. Callers typically map this to an actionable "already running"
    /// message and a supervisor-retryable exit code.
    AlreadyBound(io::Error),
    /// Any other bind failure (permissions, missing directory, etc.).
    Other(io::Error),
}

/// Bind `socket_path` as a v2 local-socket listener, enforcing
/// exactly-one-bind-per-name (the per-user-session singleton property).
///
/// **Never unlinks the path up front.** An earlier version of this logic
/// (duplicated in `running-process-broker-v2::main` before this
/// extraction) unconditionally ran `remove_file` before every bind
/// attempt on Unix. Under a real concurrent-start race that let every one
/// of N racing starters delete the current winner's *live* socket and
/// rebind over the freed path — so all N starters observed a successful
/// bind instead of exactly one (running-process#899, soldr#2361/#2363's
/// singleton testing invariant). This function instead attempts the bind
/// first with no cleanup. On an already-bound stale result, it serializes
/// contenders with a sidecar lock, retries the bind under that lock, and
/// only lets the holder that still sees a stale endpoint remove and reclaim
/// it. Windows needs no cleanup step at all — the named pipe namespace is
/// kernel-managed and a prior binding vanishes when that process exits.
///
/// On Unix, the parent directory of `socket_path` is created if missing
/// before the first bind attempt.
pub fn bind_singleton(socket_path: &str) -> Result<Listener, BindSingletonError> {
    let endpoint = crate::platform::ipc::Endpoint::new(socket_path.to_owned())
        .map_err(|error| BindSingletonError::InvalidName(error.to_string()))?;
    bind_singleton_with_endpoint(&endpoint, || Listener::bind(&endpoint))
}

/// Bind a caller-owned listener with the same singleton and serialized stale
/// recovery contract as [`bind_singleton`].
///
/// This is for consumers that need listener options the default synchronous
/// [`Listener`] does not expose (for example an async listener or a custom
/// accept backlog). `bind` must attempt to claim `socket_path` without
/// unlinking or reclaiming it first. It may be called up to three times: the
/// ordinary bind, a serialized recheck after another contender may have
/// recovered the path, and one final bind after this contender retires a
/// still-stale endpoint.
pub fn bind_singleton_with<T, F>(socket_path: &str, bind: F) -> Result<T, BindSingletonError>
where
    F: FnMut() -> io::Result<T>,
{
    let endpoint = crate::platform::ipc::Endpoint::new(socket_path.to_owned())
        .map_err(|error| BindSingletonError::InvalidName(error.to_string()))?;
    bind_singleton_with_endpoint(&endpoint, bind)
}

fn bind_singleton_with_endpoint<T, F>(
    endpoint: &crate::platform::ipc::Endpoint,
    mut bind: F,
) -> Result<T, BindSingletonError>
where
    F: FnMut() -> io::Result<T>,
{
    endpoint
        .ensure_parent_exists()
        .map_err(BindSingletonError::Other)?;
    let mut listener_result = bind();

    if let Err(err) = &listener_result {
        if is_already_bound_error(err) && endpoint.is_stale() {
            listener_result = recover_stale_endpoint(endpoint, &mut bind);
        }
    }

    listener_result.map_err(|err| {
        if is_already_bound_error(&err) {
            BindSingletonError::AlreadyBound(err)
        } else {
            BindSingletonError::Other(err)
        }
    })
}

/// Serialize the destructive part of stale-endpoint recovery.
///
/// Every contender first failed the ordinary bind and observed an orphaned
/// filesystem socket. Without a separate lock they can all make that decision,
/// then take turns unlinking whichever endpoint currently occupies the path --
/// including a live listener installed by an earlier contender. Once the lock
/// is held, bind again before retiring anything: a prior recovery winner is now
/// reported as already bound, while a still-stale endpoint is retired exactly
/// once and rebound by this holder.
fn recover_stale_endpoint<T, F>(
    endpoint: &crate::platform::ipc::Endpoint,
    bind: &mut F,
) -> io::Result<T>
where
    F: FnMut() -> io::Result<T>,
{
    let _guard = StaleRecoveryLock::acquire(endpoint.display())?;

    match bind() {
        Ok(listener) => Ok(listener),
        Err(error) if is_already_bound_error(&error) && endpoint.is_stale() => {
            endpoint.retire()?;
            bind()
        }
        Err(error) => Err(error),
    }
}

struct StaleRecoveryLock(File);

impl StaleRecoveryLock {
    fn acquire(socket_path: &str) -> io::Result<Self> {
        let lock_path = stale_recovery_lock_path(socket_path);
        let file = crate::platform::fs::open_lock_file(&lock_path)?;
        let deadline = Instant::now() + STALE_RECOVERY_LOCK_TIMEOUT;
        loop {
            match crate::platform::fs::try_lock_exclusive(&file) {
                Ok(()) => return Ok(Self(file)),
                Err(error)
                    if crate::platform::fs::is_lock_conflict(&error)
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(STALE_RECOVERY_LOCK_POLL);
                }
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for StaleRecoveryLock {
    fn drop(&mut self) {
        let _ = crate::platform::fs::unlock(&self.0);
    }
}

fn stale_recovery_lock_path(socket_path: &str) -> PathBuf {
    let mut lock_path = std::ffi::OsString::from(socket_path);
    lock_path.push(".bind.lock");
    PathBuf::from(lock_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_socket_path_produces_a_nonempty_path() {
        let path = resolve_socket_path("rpb-v2-test-singleton-bind").expect("resolve");
        assert!(!path.is_empty());
    }

    #[test]
    fn path_scoped_socket_does_not_add_a_user_runtime_directory() {
        let first = resolve_path_scoped_socket_path("rpb-v2-program-0123456789abcdef-0")
            .expect("resolve path-scoped endpoint");
        let again = resolve_path_scoped_socket_path("rpb-v2-program-0123456789abcdef-0")
            .expect("resolve stable endpoint");
        assert_eq!(first, again);
        if crate::platform::ipc::endpoint_is_filesystem_backed() {
            assert_eq!(
                std::path::Path::new(&first).parent(),
                Some(std::path::Path::new("/tmp"))
            );
        }
    }

    #[test]
    fn is_already_bound_error_classifies_expected_kinds() {
        assert!(is_already_bound_error(&io::Error::from(
            io::ErrorKind::AddrInUse
        )));
        assert!(is_already_bound_error(&io::Error::from(
            io::ErrorKind::WouldBlock
        )));
        // PR #536 deliberately added `PermissionDenied` to this matcher:
        // on Windows, a double-bind surfaces as `ERROR_ACCESS_DENIED`
        // (raw os error 5) because the existing pipe instance's ACL
        // blocks the second bind -- not as `AddrInUse`. An earlier
        // version of this test (PR #534, before the classification was
        // widened) expected the negation; PR #536 updated the impl but
        // forgot the test, which then cascade-failed every CI run until
        // fixed.
        assert!(is_already_bound_error(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
        assert!(!is_already_bound_error(&io::Error::from(
            io::ErrorKind::NotFound
        )));
    }

    #[test]
    fn bind_singleton_binds_once_and_refuses_a_second_bind() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let socket_path = resolve_socket_path(&format!(
            "rpb-v2-test-singleton-bind-{:010x}",
            nonce & 0xFF_FFFF_FFFF
        ))
        .expect("resolve");

        let _first = bind_singleton(&socket_path).expect("first bind must succeed");
        let second = bind_singleton(&socket_path);
        assert!(
            matches!(second, Err(BindSingletonError::AlreadyBound(_))),
            "second bind at the same path must be refused as AlreadyBound, got {second:?}"
        );
    }

    #[test]
    fn stale_endpoint_n_way_recovery_has_exactly_one_winner() {
        use std::sync::{mpsc, Arc, Barrier};

        const CONTENDERS: usize = 16;

        if !crate::platform::ipc::endpoint_is_filesystem_backed() {
            return;
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("stale.sock");
        let socket_path = socket_path.to_string_lossy().into_owned();
        let endpoint = crate::platform::ipc::Endpoint::new(socket_path.clone())
            .expect("construct stale endpoint");
        let mut stale_listener = Listener::bind(&endpoint).expect("seed stale endpoint");
        stale_listener.do_not_reclaim_name_on_drop();
        drop(stale_listener);
        assert!(endpoint.is_stale(), "seeded endpoint must be stale");

        let start = Arc::new(Barrier::new(CONTENDERS));
        let release = Arc::new(Barrier::new(CONTENDERS + 1));
        let (send, receive) = mpsc::channel();
        let threads: Vec<_> = (0..CONTENDERS)
            .map(|_| {
                let socket_path = socket_path.clone();
                let start = Arc::clone(&start);
                let release = Arc::clone(&release);
                let send = send.clone();
                std::thread::spawn(move || {
                    start.wait();
                    let listener = bind_singleton_with(&socket_path, || {
                        let endpoint = crate::platform::ipc::Endpoint::new(socket_path.clone())?;
                        Listener::bind(&endpoint)
                    });
                    send.send(listener.is_ok()).expect("send bind result");
                    release.wait();
                    drop(listener);
                })
            })
            .collect();
        drop(send);

        let results: Vec<_> = receive.iter().take(CONTENDERS).collect();
        assert_eq!(
            results.iter().filter(|won| **won).count(),
            1,
            "stale recovery must not unlink a newly bound winner: {results:?}"
        );

        release.wait();
        for thread in threads {
            thread.join().expect("bind contender");
        }
    }
}
