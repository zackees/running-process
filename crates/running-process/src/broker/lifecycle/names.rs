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

use crate::broker::lifecycle::sid::SidError;

/// Errors that prevent computing a valid pipe path.
#[derive(Debug, thiserror::Error)]
pub enum PipePathError {
    /// A name argument failed regex validation.
    #[error("invalid name {name:?}: {reason}")]
    InvalidName {
        /// The offending input.
        name: String,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// The derived path exceeds a platform-specific bound.
    #[error("derived path exceeds {limit_label} ({len} > {max})")]
    PathTooLong {
        /// Length we tried to produce.
        len: usize,
        /// Platform-specific cap.
        max: usize,
        /// "Windows MAX_PATH" / "macOS sun_path" / etc.
        limit_label: &'static str,
    },

    /// Failure to compute the per-user SID hash.
    #[error(transparent)]
    Sid(#[from] SidError),
}

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

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate a service name against `[a-z0-9-]{1,64}`.
///
/// Exposed for callers that want to validate user input before
/// computing the pipe name (so they can surface a friendlier error).
pub fn validate_service_name(name: &str) -> Result<(), PipePathError> {
    if name.is_empty() {
        return Err(PipePathError::InvalidName {
            name: name.into(),
            reason: "service name must be at least 1 character",
        });
    }
    if name.len() > 64 {
        return Err(PipePathError::InvalidName {
            name: name.into(),
            reason: "service name must be 64 characters or fewer",
        });
    }
    for c in name.chars() {
        match c {
            'a'..='z' | '0'..='9' | '-' => {}
            'A'..='Z' => {
                // Case-only collision guard — see module docs.
                return Err(PipePathError::InvalidName {
                    name: name.into(),
                    reason: "uppercase letters are forbidden (case-only \
                             collisions with lowercase names would silently \
                             merge under Windows named-pipe semantics)",
                });
            }
            _ => {
                return Err(PipePathError::InvalidName {
                    name: name.into(),
                    reason: "only lowercase ASCII letters, digits, and '-' allowed",
                });
            }
        }
    }
    Ok(())
}

/// Validate a semver-like version string against
/// `^[0-9]+\.[0-9]+\.[0-9]+(-[a-z0-9.]+)?$`.
///
/// Used by callers that want to render `{service}-{version}` into a
/// pipe name themselves (the helpers here keep the name format flat,
/// but the validator is exposed for the broker-side dispatch table).
pub fn validate_version(version: &str) -> Result<(), PipePathError> {
    if version.is_empty() {
        return Err(PipePathError::InvalidName {
            name: version.into(),
            reason: "version must not be empty",
        });
    }
    // Split off pre-release tail.
    let (core, prerelease) = match version.split_once('-') {
        Some((core, tail)) => (core, Some(tail)),
        None => (version, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return Err(PipePathError::InvalidName {
            name: version.into(),
            reason: "version core must be MAJOR.MINOR.PATCH",
        });
    }
    for p in &parts {
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
            return Err(PipePathError::InvalidName {
                name: version.into(),
                reason: "MAJOR/MINOR/PATCH must be non-empty digits",
            });
        }
    }
    if let Some(tail) = prerelease {
        if tail.is_empty() {
            return Err(PipePathError::InvalidName {
                name: version.into(),
                reason: "pre-release suffix after '-' must not be empty",
            });
        }
        for c in tail.chars() {
            match c {
                'a'..='z' | '0'..='9' | '.' => {}
                _ => {
                    return Err(PipePathError::InvalidName {
                        name: version.into(),
                        reason: "pre-release tail allows only [a-z0-9.]",
                    });
                }
            }
        }
    }
    Ok(())
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
