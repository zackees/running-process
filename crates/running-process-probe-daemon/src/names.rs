//! Socket naming and path resolution for the probe daemon.
//!
//! Mirrors the broker's `names_v2` shape with a probe-specific prefix rather
//! than editing `names_v2.rs` — the broker's naming is its own contract and
//! the probe daemon is a separate service that merely follows the convention.

use std::io;

/// Distinguishes probe daemon endpoints from broker ones in the same
/// namespace. A collision would let a client dial the wrong service.
const PIPE_PREFIX_PROBE: &str = "rpp-probe";

/// `rpp-probe-{sid_hash}-{idx}`.
///
/// `sid_hash` scopes the name to the current user so two users on one machine
/// never contend for the same endpoint.
pub fn probe_pipe_name(sid_hash: &str, idx: u32) -> String {
    format!("{PIPE_PREFIX_PROBE}-{sid_hash}-{idx}")
}

/// The endpoint component the platform facade files probe sockets under.
///
/// Keeps probe sockets in their own per-user runtime directory, apart from the
/// broker's, so the two services can never contend for a path.
pub(crate) const ENDPOINT_COMPONENT: &str = "probe";

/// Turn a bare endpoint name into the platform's concrete socket path.
///
/// The host decides directory placement and the leaf spelling (a pipe path on
/// Windows, a per-user runtime directory on Unix, a hashed leaf on macOS to fit
/// `sun_path`); this crate owns only the `probe` component and the name prefix.
pub fn resolve_socket_path(bare_name: &str) -> String {
    running_process_platform_internal::platform::ipc::component_endpoint_path(
        ENDPOINT_COMPONENT,
        bare_name,
    )
}

/// Wrap a resolved path in interprocess's platform-appropriate `Name`.
pub fn wrap_socket_name(socket_path: &str) -> io::Result<interprocess::local_socket::Name<'_>> {
    use interprocess::local_socket::prelude::*;
    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        let bare = socket_path
            .strip_prefix(r"\\.\pipe\")
            .unwrap_or(socket_path);
        bare.to_ns_name::<GenericNamespaced>()
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        socket_path.to_fs_name::<GenericFilePath>()
    }
}

/// Classify a bind failure as "someone else already owns this endpoint".
///
/// `AddrInUse` / `WouldBlock` are the canonical signals. **Windows named-pipe
/// double-bind surfaces as `PermissionDenied`** (ERROR_ACCESS_DENIED) because
/// the existing instance's ACL rejects the second bind — omitting it would
/// make a losing race look like a hard failure. Matches the broker-v2
/// classifier for the same reason.
pub fn is_already_bound_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::AddrInUse | io::ErrorKind::WouldBlock | io::ErrorKind::PermissionDenied
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_carries_prefix_sid_and_index() {
        assert_eq!(
            probe_pipe_name("0123456789abcdef", 0),
            "rpp-probe-0123456789abcdef-0"
        );
    }

    #[test]
    fn pipe_name_is_distinct_per_user() {
        assert_ne!(probe_pipe_name("aaaa", 0), probe_pipe_name("bbbb", 0));
    }

    #[test]
    fn already_bound_covers_the_three_race_signals() {
        for kind in [
            io::ErrorKind::AddrInUse,
            io::ErrorKind::WouldBlock,
            io::ErrorKind::PermissionDenied,
        ] {
            assert!(
                is_already_bound_error(&io::Error::new(kind, "x")),
                "{kind:?} must count as already-bound"
            );
        }
    }

    #[test]
    fn already_bound_does_not_swallow_unrelated_errors() {
        assert!(!is_already_bound_error(&io::Error::new(
            io::ErrorKind::NotFound,
            "missing"
        )));
    }

    #[test]
    fn resolved_path_contains_the_bare_name_or_a_hash() {
        let p = resolve_socket_path("rpp-probe-deadbeef-0");
        assert!(!p.is_empty());
        if cfg!(target_os = "macos") {
            assert!(p.ends_with(".sock"));
        } else {
            assert!(p.contains("rpp-probe-deadbeef-0"));
        }
    }
}
