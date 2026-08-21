//! v2 broker pipe-name derivation (slice 3b of #483).
//!
//! Mirrors [`super::names`] for the v2 broker, using a different
//! namespace prefix (`rpb-v2-` instead of `rpb-v1-`) so a v1 and v2
//! broker for the same program can coexist on one machine. Per #470's
//! coexistence table, v2 ships as a parallel stack alongside v1 during
//! rollout; matching the v1 module's API shape lets later slices and
//! downstream consumers (zccache, etc.) port surfaces one at a time
//! without learning a new naming model.
//!
//! Slice 3b exposes only [`v2_program_pipe`] — the per-program pipe
//! name a v2 broker binds and a v2 client dials. Subsequent slices add
//! more names (private/shared/explicit-instance counterparts, the
//! broker↔daemon transport name) as they are needed.

use std::path::{Path, PathBuf};

use crate::broker::lifecycle::names::{validate_service_name, PipePathError};

/// Compile-time prefix for every v2 broker pipe. Counterpart of the
/// frozen v1 `PIPE_PREFIX = "rpb-v1"`. Encodes the v2 envelope version
/// so v1 and v2 brokers can bind simultaneously without colliding.
const PIPE_PREFIX_V2: &str = "rpb-v2";

/// Failure to turn an installed broker executable path into an IPC scope.
#[derive(Debug, thiserror::Error)]
pub enum BrokerPathIdentityError {
    /// The executable path must exist so every process hashes the same
    /// canonical filesystem identity. There is deliberately no lexical-path
    /// fallback: disagreement here would send clients to a different pipe.
    #[error("canonicalize installed broker path {path:?}: {source}")]
    Canonicalize {
        /// Broker executable path supplied by the caller.
        path: PathBuf,
        /// Filesystem error returned by canonicalization.
        #[source]
        source: std::io::Error,
    },
}

/// Derive the 16-hex IPC scope from the canonical installed broker path.
///
/// The path itself is the scope contract. A per-user installation naturally
/// contains that user's private install path, while every caller of a
/// machine-wide installation sees the same path and therefore the same pipe.
/// No SID, machine-id, registry mapping, or fallback namespace participates.
pub fn broker_path_scope_hash(
    broker_path: impl AsRef<Path>,
) -> Result<String, BrokerPathIdentityError> {
    let supplied = broker_path.as_ref();
    let canonical = std::fs::canonicalize(supplied).map_err(|source| {
        BrokerPathIdentityError::Canonicalize {
            path: supplied.to_path_buf(),
            source,
        }
    })?;

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"running-process:broker-install-path:v1\0");
    // The host decides which spelling differences are meaningless; this
    // module owns the domain separator and the encoding below.
    hasher.update(&crate::platform::ipc::endpoint_scope_bytes(&canonical));

    let digest = hasher.finalize();
    let mut scope = String::with_capacity(16);
    for byte in digest.as_bytes().iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(scope, "{byte:02x}");
    }
    Ok(scope)
}

/// Compute the v2 pipe name for one installed broker executable.
///
/// This is the path-scoped counterpart of [`v2_program_pipe`]. Both broker
/// and client should call it with the same canonical installed executable;
/// the resulting endpoint contains no per-user SID component.
pub fn v2_broker_path_pipe(
    program: &str,
    broker_path: impl AsRef<Path>,
    pipe_idx: u32,
) -> Result<String, BrokerPathPipeError> {
    let scope = broker_path_scope_hash(broker_path)?;
    Ok(v2_program_pipe(program, &scope, pipe_idx)?)
}

/// Error returned while deriving a path-scoped v2 pipe name.
#[derive(Debug, thiserror::Error)]
pub enum BrokerPathPipeError {
    /// Installed broker path identity could not be resolved exactly.
    #[error(transparent)]
    Identity(#[from] BrokerPathIdentityError),
    /// The program/scope combination was not a valid v2 pipe name.
    #[error(transparent)]
    Pipe(#[from] PipePathError),
}

/// Compute the v2 per-program pipe name.
///
/// Returns `"rpb-v2-{program}-{sid_hash}-{pipe_idx}"` after validating
/// `program` against the same `[a-z0-9-]{1,64}` rule as v1 service
/// names (case-only collisions are rejected for the same Windows
/// named-pipe reason documented on v1's [`validate_service_name`]) and
/// `sid_hash` for non-emptiness + 16-char hex shape.
///
/// `pipe_idx` is included so a v2 broker can bind multiple acceptor
/// pipes (`-0`, `-1`, ...) for fanout, mirroring the v1 pattern
/// `rpb-v1-<program>-<sid_hash>-<pipe_idx>` documented in #470.
///
/// This slice returns just the canonical name string. Wrapping that
/// into a platform-neutral `PipePath` (Windows `\\.\pipe\…` vs Unix
/// socket file under the broker shadow dir) lands in slice 3c when the
/// v2 binary actually starts binding.
pub fn v2_program_pipe(
    program: &str,
    sid_hash: &str,
    pipe_idx: u32,
) -> Result<String, PipePathError> {
    validate_service_name(program)?;
    validate_sid_hash(sid_hash)?;
    Ok(format!("{PIPE_PREFIX_V2}-{program}-{sid_hash}-{pipe_idx}"))
}

/// Validate that `sid_hash` is exactly 16 lowercase hex characters —
/// the same shape produced by [`super::sid::user_sid_hash`] /
/// [`super::sid::hash_to_16_hex`].
fn validate_sid_hash(sid_hash: &str) -> Result<(), PipePathError> {
    if sid_hash.is_empty() {
        return Err(PipePathError::InvalidName {
            name: sid_hash.into(),
            reason: "sid_hash must be at least 1 character",
        });
    }
    if sid_hash.len() != 16 {
        return Err(PipePathError::InvalidName {
            name: sid_hash.into(),
            reason: "sid_hash must be exactly 16 hex characters",
        });
    }
    for c in sid_hash.chars() {
        if !c.is_ascii_hexdigit() || c.is_ascii_uppercase() {
            return Err(PipePathError::InvalidName {
                name: sid_hash.into(),
                reason: "sid_hash must be lowercase hex digits",
            });
        }
    }
    Ok(())
}

/// Directory holding a v2 broker's per-user runtime state.
///
/// # Why this exists as one function
///
/// On Unix the broker's socket already lives in a directory, so runtime
/// state had an implicit home. On Windows the socket is a named pipe in a
/// kernel namespace with no directory at all — so anything that must be a
/// *file* (the HTTP endpoint published by [`super::super::broker_http_discovery`],
/// for one) had nowhere agreed to live.
///
/// A publisher and a reader that each derive that location independently
/// will eventually disagree, and the failure is silent: the reader simply
/// reports "no broker running" forever. Both sides call this instead.
///
/// The directory is not created here. Callers that write into it create it
/// owner-only at that point; callers that only read must treat an absent
/// directory as "nothing published", which is a normal state.
///
/// # Why no `getuid()`
///
/// The obvious way to keep two users on one host apart is a uid in the path.
/// Every branch instead lands inside a location the OS already scopes to one
/// user -- `XDG_RUNTIME_DIR`, macOS's per-user `TMPDIR`, `LOCALAPPDATA`, or
/// the per-user cache directory. Separation comes from the base directory
/// rather than from a uid spelled into the leaf, and the file itself is
/// written owner-only by `broker_http_discovery::publish_http_port`.
///
/// That choice originally also kept `libc::getuid`, an `unsafe` call, out of
/// `crates/running-process/src/broker/` and its reviewed unsafe inventory
/// (`tests/security/unsafe_inventory.rs`). The placement now lives behind
/// `platform::ipc`, so that particular pressure no longer applies -- but the
/// design is retained deliberately: an OS-scoped base directory needs no
/// privilege to read and cannot be spoofed by a caller supplying a uid.
///
/// A consequence worth stating: this is *not* guaranteed to be the same
/// directory the Unix socket lives in. It is the agreed home for broker-v2
/// runtime *files*, which is all the publisher and reader need to share.
pub fn broker_v2_runtime_dir() -> std::path::PathBuf {
    crate::platform::ipc::broker_v2_runtime_dir()
}

/// Path of the identity file a daemon publishes for `service`.
///
/// # Why the service name is the key
///
/// The broker resolves a Hello by `service_name` and knows nothing else about
/// the daemon behind it. The daemon, in turn, is parameterised by *scope* and
/// has no inherent notion of which service it serves. Those two facts left no
/// shared identifier between them, which is what blocked backend-pipe
/// resolution (running-process#532 item 5) — not the choice of directory.
///
/// So the service name is supplied to the daemon explicitly (`--service`) and
/// used as the key here. Both sides call this function rather than building
/// the path themselves: a publisher and a reader that each derive it
/// independently will eventually disagree, and the failure is silent — the
/// broker simply reports the daemon as absent forever.
///
/// The file is not created here, and a missing file is a normal state
/// meaning "no daemon has published for this service".
pub fn daemon_identity_path(service: &str) -> std::path::PathBuf {
    broker_v2_runtime_dir().join(format!("daemon-{service}.json"))
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A relative path would resolve against the current directory, so a
    /// broker and a client started from different working directories would
    /// silently use different files.
    #[test]
    fn the_runtime_dir_is_absolute() {
        let dir = broker_v2_runtime_dir();
        assert!(dir.is_absolute(), "{} is not absolute", dir.display());
    }

    /// The publisher and the reader are separate calls. If this were not
    /// stable, one would write where the other never looks.
    #[test]
    fn the_runtime_dir_is_stable_across_calls() {
        assert_eq!(broker_v2_runtime_dir(), broker_v2_runtime_dir());
    }

    /// Must not collide with v1 or the daemon's state: two brokers sharing a
    /// directory would each overwrite the other's published endpoint.
    #[test]
    fn the_runtime_dir_is_specific_to_broker_v2() {
        let dir = broker_v2_runtime_dir();
        let text = dir.to_string_lossy();
        assert!(
            text.contains("broker-v2"),
            "{text} does not name broker-v2, so it may be shared with other components"
        );
    }

    const VALID_SID: &str = "deadbeefcafef00d";

    /// Publisher and reader must land on the same file. They call this from
    /// different processes, so a difference here is invisible until the
    /// broker reports a running daemon as absent.
    #[test]
    fn the_identity_path_is_stable_and_service_specific() {
        let a = daemon_identity_path("zccache");
        assert_eq!(a, daemon_identity_path("zccache"));
        assert_ne!(a, daemon_identity_path("fbuild"));
        assert!(a.is_absolute(), "{} is not absolute", a.display());
        assert!(a.starts_with(broker_v2_runtime_dir()));
        assert!(
            a.to_string_lossy().contains("zccache"),
            "{} does not name the service",
            a.display()
        );
    }

    #[test]
    fn v2_program_pipe_happy_path() {
        let name =
            v2_program_pipe("zccache", VALID_SID, 0).expect("valid inputs produce a v2 pipe name");
        assert_eq!(name, "rpb-v2-zccache-deadbeefcafef00d-0");
    }

    #[test]
    fn v2_program_pipe_distinct_pipe_idx_distinct_names() {
        let name_0 = v2_program_pipe("zccache", VALID_SID, 0).expect("idx=0 valid");
        let name_7 = v2_program_pipe("zccache", VALID_SID, 7).expect("idx=7 valid");
        assert_ne!(name_0, name_7);
        assert!(name_7.ends_with("-7"));
    }

    #[test]
    fn v2_program_pipe_rejects_invalid_program() {
        // Empty program name.
        assert!(matches!(
            v2_program_pipe("", VALID_SID, 0),
            Err(PipePathError::InvalidName { .. })
        ));
        // Uppercase (case-only collision risk on Windows).
        assert!(matches!(
            v2_program_pipe("Zccache", VALID_SID, 0),
            Err(PipePathError::InvalidName { .. })
        ));
        // 65 characters (over the v1-derived length cap).
        let too_long = "a".repeat(65);
        assert!(matches!(
            v2_program_pipe(&too_long, VALID_SID, 0),
            Err(PipePathError::InvalidName { .. })
        ));
    }

    #[test]
    fn v2_program_pipe_rejects_invalid_sid_hash() {
        // Empty sid_hash.
        assert!(matches!(
            v2_program_pipe("zccache", "", 0),
            Err(PipePathError::InvalidName { .. })
        ));
        // Wrong length (15 chars).
        assert!(matches!(
            v2_program_pipe("zccache", "deadbeefcafef00", 0),
            Err(PipePathError::InvalidName { .. })
        ));
        // Non-hex character.
        assert!(matches!(
            v2_program_pipe("zccache", "deadbeefcafef00g", 0),
            Err(PipePathError::InvalidName { .. })
        ));
        // Uppercase hex (not the canonical shape).
        assert!(matches!(
            v2_program_pipe("zccache", "DEADBEEFCAFEF00D", 0),
            Err(PipePathError::InvalidName { .. })
        ));
    }

    #[test]
    fn broker_path_scope_is_stable_and_path_specific() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = temp.path().join("broker-a");
        let second = temp.path().join("broker-b");
        std::fs::write(&first, b"a").expect("first broker fixture");
        std::fs::write(&second, b"b").expect("second broker fixture");

        let a = broker_path_scope_hash(&first).expect("first scope");
        assert_eq!(a, broker_path_scope_hash(&first).expect("stable scope"));
        assert_ne!(a, broker_path_scope_hash(&second).expect("second scope"));
        assert_eq!(a.len(), 16);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn path_scoped_pipe_contains_no_user_identity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let broker = temp.path().join("soldr");
        std::fs::write(&broker, b"broker").expect("broker fixture");
        let scope = broker_path_scope_hash(&broker).expect("scope");

        let name = v2_broker_path_pipe("soldr-daemon", &broker, 1).expect("pipe");
        assert_eq!(name, format!("rpb-v2-soldr-daemon-{scope}-1"));
    }

    #[test]
    fn missing_broker_path_has_no_lexical_fallback() {
        let missing = std::env::temp_dir().join(format!(
            "running-process-missing-broker-{}",
            std::process::id()
        ));
        let err = broker_path_scope_hash(&missing).expect_err("missing path must fail");
        assert!(matches!(err, BrokerPathIdentityError::Canonicalize { .. }));
    }
}
