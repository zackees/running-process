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

use std::io;

use interprocess::local_socket::{Listener, ListenerOptions};

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

/// Classify a [`ListenerOptions::create_sync`] error as "another process is
/// already bound at this name" vs any other bind failure.
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

/// Unix-only: tell a genuinely orphaned unix-socket path (left behind by a
/// process that exited without cleaning up) apart from a path where a live
/// peer is listening right now — the two look identical to `bind`
/// (`AddrInUse` either way). A connect probe distinguishes them: nothing is
/// listening if the connect itself fails to even reach a peer
/// (`ConnectionRefused` — the classic "orphaned socket file, no listener"
/// signal — or `NotFound`); any other outcome, including a successful
/// connect, means treat the path as live and leave it alone.
#[cfg(unix)]
pub fn unix_socket_path_is_stale(socket_path: &str) -> bool {
    use interprocess::local_socket::traits::Stream as _;
    use interprocess::local_socket::Stream;
    let Ok(name) = wrap_socket_name(socket_path) else {
        return false; // can't even build the name -- don't touch the file
    };
    match Stream::connect(name) {
        Ok(_stream) => false,
        Err(err) => matches!(
            err.kind(),
            io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
        ),
    }
}

/// Build an `interprocess` [`Name`](interprocess::local_socket::Name) from a
/// resolved socket path (see [`resolve_socket_path`]).
pub fn wrap_socket_name(socket_path: &str) -> Result<interprocess::local_socket::Name<'_>, String> {
    use interprocess::local_socket::prelude::*;
    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        let bare = socket_path
            .strip_prefix(r"\\.\pipe\")
            .unwrap_or(socket_path);
        bare.to_ns_name::<GenericNamespaced>()
            .map_err(|e| format!("to_ns_name: {e}"))
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        socket_path
            .to_fs_name::<GenericFilePath>()
            .map_err(|e| format!("to_fs_name: {e}"))
    }
}

/// Why [`bind_singleton`] refused to bind.
#[derive(Debug)]
pub enum BindSingletonError {
    /// Building the platform [`Name`](interprocess::local_socket::Name)
    /// from `socket_path` failed.
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
/// singleton testing invariant). This function instead: attempts the bind
/// first with no cleanup; on an already-bound failure, Unix-only,
/// connect-probes the path via `unix_socket_path_is_stale` (Unix-only, so
/// not linked here — a doc build on a non-Unix target has no such item in
/// scope to resolve against) to tell a
/// genuinely orphaned socket file apart from a live peer, and only then
/// removes + retries once. Windows needs no cleanup step at all — the
/// named pipe namespace is kernel-managed and a prior binding vanishes
/// when that process exits.
///
/// On Unix, the parent directory of `socket_path` is created if missing
/// before the first bind attempt.
pub fn bind_singleton(socket_path: &str) -> Result<Listener, BindSingletonError> {
    #[cfg(unix)]
    {
        let path = std::path::Path::new(socket_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(BindSingletonError::Other)?;
        }
    }

    let name = wrap_socket_name(socket_path).map_err(BindSingletonError::InvalidName)?;
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut listener_result = ListenerOptions::new().name(name).create_sync();

    #[cfg(unix)]
    if let Err(err) = &listener_result {
        if is_already_bound_error(err) && unix_socket_path_is_stale(socket_path) {
            let _ = std::fs::remove_file(socket_path);
            listener_result = match wrap_socket_name(socket_path) {
                Ok(retry_name) => ListenerOptions::new().name(retry_name).create_sync(),
                Err(_) => listener_result,
            };
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
        #[cfg(unix)]
        assert_eq!(
            std::path::Path::new(&first).parent(),
            Some(std::path::Path::new("/tmp"))
        );
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
}
