//! Canonical v1 broker pipe-name derivation.
//!
//! Phase 1 of #228 (issue #230). Every name is derived from the
//! caller's [`user_sid_hash`](super::sid::user_sid_hash) plus a few
//! frozen string templates. The Windows form is a named pipe
//! (`\\.\pipe\...`); the Unix form is a filesystem socket path under
//! the broker shadow directory.
//!
//! The four canonical names exposed here are:
//!
//! | Function                  | Purpose                                                             |
//! |---------------------------|---------------------------------------------------------------------|
//! | [`shared_broker_pipe`]    | Single per-user broker that serves every service together.          |
//! | [`private_broker_pipe`]   | Service-isolated broker (e.g. one zccache instance only).           |
//! | [`explicit_instance_pipe`]| Hand-named broker for tests/dev/multi-instance scenarios.           |
//! | [`backend_pipe`]          | The per-backend handle the broker hands a client after negotiation. |
//!
//! ## Validation
//!
//! Service names must match `[a-z0-9-]{1,64}`. Version strings must
//! match a semver-like `^[0-9]+\.[0-9]+\.[0-9]+(-[a-z0-9.]+)?$`.
//! Explicit instance names match `[a-z0-9-]{1,64}`. Case-only
//! collisions (`Zccache` vs `zccache`) are rejected with
//! [`PipePathError::InvalidName`] because Windows named pipes are
//! case-insensitive and silently coalescing would let a malicious
//! caller hijack a legitimate broker.
//!
//! ## Length limits
//!
//! - Windows `\\.\pipe\` names without the `\\?\` long-path prefix
//!   are capped by `MAX_PATH = 260` characters.
//! - macOS `sun_path` (the path field of `struct sockaddr_un`) is 104
//!   bytes. The Unix path returned here is validated to stay under
//!   that bound after combining `shadow_dir() + "/broker/" + name +
//!   ".sock"`.

use std::path::PathBuf;

// The v1 manifest/service registry and the legacy broker pipe builders use
// one validation/error type.  `client` composes `daemon-registration`, so
// this remains the literal same public type on the established broker path.
pub use crate::daemon_registration::validation::{
    validate_service_name, validate_version, PipePathError,
};

/// A pipe address in platform-neutral form.
///
/// Exactly one of [`Self::windows`] or [`Self::unix`] is populated on
/// any given host. The other field is `None`. Which one is populated
/// follows [`crate::platform::ipc::endpoint_is_filesystem_backed`], so
/// callers select the active value without naming a host themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipePath {
    /// Windows named-pipe path (e.g. `\\.\pipe\rpb-v1-abc-shared`).
    pub windows: Option<String>,
    /// Unix domain socket path (e.g.
    /// `/run/user/1000/running-process/broker/rpb-v1-abc-shared.sock`).
    pub unix: Option<PathBuf>,
}

/// Windows MAX_PATH ceiling without the `\\?\` long-path prefix.
pub const WINDOWS_MAX_PATH: usize = 260;

/// macOS `sun_path` field ceiling. POSIX requires at least 92;
/// Darwin's `struct sockaddr_un` actually has 104.
pub const MACOS_SUN_PATH_MAX: usize = 104;

/// Linux `sun_path` field ceiling. glibc defines it as 108.
pub const LINUX_SUN_PATH_MAX: usize = 108;

/// Compile-time prefix every broker pipe shares. Encodes the v1
/// envelope version and the "running-process broker" namespace so
/// pipe names cannot accidentally collide with anything else under
/// `\\.\pipe\` or `shadow_dir()/broker/`.
const PIPE_PREFIX: &str = "rpb-v1";

/// Compute the shared-broker pipe address.
///
/// The shared broker is the default: one instance per user that fans
/// every service request out to the right backend.
pub fn shared_broker_pipe(user_sid_hash: &str) -> Result<PipePath, PipePathError> {
    validate_sid_hash(user_sid_hash)?;
    build_pipe_path(&format!("{PIPE_PREFIX}-{user_sid_hash}-shared"))
}

/// Compute the private-broker pipe address for a single service.
///
/// Service names must match `[a-z0-9-]{1,64}`.
pub fn private_broker_pipe(user_sid_hash: &str, service: &str) -> Result<PipePath, PipePathError> {
    validate_sid_hash(user_sid_hash)?;
    validate_service_name(service)?;
    build_pipe_path(&format!("{PIPE_PREFIX}-{user_sid_hash}-svc-{service}"))
}

/// Compute the explicit-instance broker pipe address.
///
/// `name` must match `[a-z0-9-]{1,64}` and is otherwise unrestricted.
/// Used for tests and multi-instance dev setups.
pub fn explicit_instance_pipe(user_sid_hash: &str, name: &str) -> Result<PipePath, PipePathError> {
    validate_sid_hash(user_sid_hash)?;
    validate_service_name(name)?; // same `[a-z0-9-]{1,64}` rule
    build_pipe_path(&format!("{PIPE_PREFIX}-{user_sid_hash}-inst-{name}"))
}

/// Compute the backend pipe address the broker hands a client after
/// Hello negotiation.
///
/// `random128` is a 16-byte (128-bit) random suffix the broker
/// generates per connection. Rendered as lowercase hex to keep the
/// pipe name in the `[a-z0-9-]` charset.
pub fn backend_pipe(user_sid_hash: &str, random128: &[u8; 16]) -> Result<PipePath, PipePathError> {
    validate_sid_hash(user_sid_hash)?;
    let mut suffix = String::with_capacity(32);
    for b in random128 {
        suffix.push(nibble_to_hex(b >> 4));
        suffix.push(nibble_to_hex(b & 0x0F));
    }
    build_pipe_path(&format!("{PIPE_PREFIX}-{user_sid_hash}-be-{suffix}"))
}

fn validate_sid_hash(s: &str) -> Result<(), PipePathError> {
    if s.len() != 16 {
        return Err(PipePathError::InvalidName {
            name: s.into(),
            reason: "user_sid_hash must be exactly 16 hex characters",
        });
    }
    for c in s.chars() {
        if !(c.is_ascii_digit() || ('a'..='f').contains(&c)) {
            return Err(PipePathError::InvalidName {
                name: s.into(),
                reason: "user_sid_hash must be lowercase hex",
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Path assembly
// ---------------------------------------------------------------------------

#[inline]
fn nibble_to_hex(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + (n - 10)) as char,
        _ => unreachable!("nibble out of range"),
    }
}

fn build_pipe_path(name: &str) -> Result<PipePath, PipePathError> {
    // The selected host owns directory placement, leaf spelling, and the
    // length budget. This module owns which bare name to ask for.
    let address = crate::platform::ipc::broker_v1_endpoint_path(name).map_err(|err| {
        PipePathError::PathTooLong {
            len: err.len,
            max: err.max,
            limit_label: err.limit_label,
        }
    })?;

    Ok(if crate::platform::ipc::endpoint_is_filesystem_backed() {
        PipePath {
            windows: None,
            unix: Some(PathBuf::from(address)),
        }
    } else {
        PipePath {
            windows: Some(address),
            unix: None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_HASH: &str = "0123456789abcdef";

    /// Return the single populated form, asserting the other is empty.
    ///
    /// Host-specific spelling of that form is characterized beside each
    /// concrete implementation in `running-process-platform-internal`;
    /// this module owns only the host-neutral contract.
    fn sole_address(path: &PipePath) -> String {
        match (&path.windows, &path.unix) {
            (Some(windows), None) => windows.clone(),
            (None, Some(unix)) => unix.to_string_lossy().into_owned(),
            _ => panic!("exactly one form must be populated"),
        }
    }

    #[test]
    fn shared_broker_pipe_builds() {
        let path = shared_broker_pipe(SAMPLE_HASH).expect("shared pipe should build");
        assert!(!sole_address(&path).is_empty());
    }

    #[test]
    fn populated_form_follows_the_selected_transport() {
        let path = shared_broker_pipe(SAMPLE_HASH).expect("shared pipe should build");
        assert_eq!(
            path.unix.is_some(),
            crate::platform::ipc::endpoint_is_filesystem_backed(),
            "filesystem-backed hosts must populate the unix form and no other"
        );
    }

    #[test]
    fn the_derived_address_respects_the_host_budget() {
        let limit = crate::platform::ipc::endpoint_name_limit();
        let path = backend_pipe(SAMPLE_HASH, &[0xABu8; 16]).expect("backend pipe");
        assert!(
            sole_address(&path).len() <= limit.max_bytes,
            "derived address exceeds the {} budget of {} bytes",
            limit.label,
            limit.max_bytes
        );
    }

    #[test]
    fn the_host_budget_is_one_of_the_documented_ceilings() {
        let limit = crate::platform::ipc::endpoint_name_limit();
        assert!(
            matches!(
                (limit.max_bytes, limit.label),
                (WINDOWS_MAX_PATH, "Windows MAX_PATH")
                    | (MACOS_SUN_PATH_MAX, "macOS sun_path")
                    | (LINUX_SUN_PATH_MAX, "Linux sun_path")
            ),
            "facade reported {} / {} bytes, which matches no documented ceiling",
            limit.label,
            limit.max_bytes
        );
    }

    #[test]
    fn private_broker_pipe_rejects_uppercase() {
        let err = private_broker_pipe(SAMPLE_HASH, "Zccache").unwrap_err();
        match err {
            PipePathError::InvalidName { .. } => {}
            _ => panic!("expected InvalidName, got {err:?}"),
        }
    }

    #[test]
    fn validate_version_accepts_semver() {
        validate_version("1.0.0").unwrap();
        validate_version("1.11.20").unwrap();
        validate_version("0.0.1-alpha.1").unwrap();
        validate_version("2.3.4-rc.1.beta").unwrap();
    }

    #[test]
    fn validate_version_rejects_invalid() {
        assert!(validate_version("").is_err());
        assert!(validate_version("1.0").is_err());
        assert!(validate_version("1.0.0.0").is_err());
        assert!(validate_version("1.0.0-").is_err());
        assert!(validate_version("1.0.0-ALPHA").is_err()); // uppercase
        assert!(validate_version("v1.0.0").is_err());
    }

    #[test]
    fn backend_pipes_are_distinct_per_random_suffix() {
        // macOS folds the canonical name into a hashed leaf, so the raw hex
        // suffix is not observable in the address on every host. Uniqueness
        // is the property every host must preserve.
        let first = backend_pipe(SAMPLE_HASH, &[0xABu8; 16]).expect("backend pipe");
        let second = backend_pipe(SAMPLE_HASH, &[0xCDu8; 16]).expect("backend pipe");
        assert_ne!(sole_address(&first), sole_address(&second));
    }
}
