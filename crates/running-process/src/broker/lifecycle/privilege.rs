//! Broker startup privilege checks.
//!
//! The broker control socket is a per-user boundary. Starting it as
//! root or Windows LocalSystem would make that boundary ambiguous, so
//! the binary refuses privileged startup unless a test environment
//! explicitly opts out.

/// Environment variable that permits privileged broker startup.
///
/// This exists for controlled test fixtures only. Production launchers
/// should run the broker as the target user instead.
pub const ALLOW_PRIVILEGED_ENV: &str = "RUNNING_PROCESS_BROKER_ALLOW_PRIVILEGED";

/// Errors returned while checking broker startup privileges.
#[derive(Debug, thiserror::Error)]
pub enum PrivilegeError {
    /// The current process is running as a privileged OS identity.
    #[error(
        "running-process-broker-v1 refuses to run as {identity} by default; set {ALLOW_PRIVILEGED_ENV}=1 only for isolated test environments"
    )]
    Privileged {
        /// Privileged identity detected for the current process.
        identity: PrivilegedIdentity,
    },
    /// The platform privilege lookup failed.
    #[error("failed to determine broker process privilege: {0}")]
    PlatformLookup(String),
}

/// Privileged identities that are forbidden for the broker by default.
///
/// Kept under this name for callers that already match on it. Which identities
/// exist, and how each is detected, is the host's answer rather than the
/// broker's; what the broker owns is that they are forbidden.
pub use crate::platform::host::PrivilegedIdentity;

/// Refuse to start the broker when the current process is privileged.
///
/// The check runs before the binary binds any socket. Set
/// [`ALLOW_PRIVILEGED_ENV`] to `1` only for isolated test environments
/// that intentionally exercise privileged startup behavior.
pub fn refuse_privileged_run() -> Result<(), PrivilegeError> {
    if allow_privileged_from_env() {
        return Ok(());
    }
    refuse_process_privilege(current_process_privilege()?)
}

fn refuse_process_privilege(identity: Option<PrivilegedIdentity>) -> Result<(), PrivilegeError> {
    match identity {
        Some(identity) => Err(PrivilegeError::Privileged { identity }),
        None => Ok(()),
    }
}

fn allow_privileged_from_env() -> bool {
    crate::env_vars::BROKER_ALLOW_PRIVILEGED.is_set()
}

fn current_process_privilege() -> Result<Option<PrivilegedIdentity>, PrivilegeError> {
    crate::platform::host::current_process_privilege()
        .map_err(|error| PrivilegeError::PlatformLookup(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env_vars::EnvKind;

    #[test]
    fn refuses_privileged_identity() {
        let err = refuse_process_privilege(Some(PrivilegedIdentity::UnixRoot)).unwrap_err();
        assert!(matches!(
            err,
            PrivilegeError::Privileged {
                identity: PrivilegedIdentity::UnixRoot
            }
        ));
    }

    #[test]
    fn allows_unprivileged_identity() {
        refuse_process_privilege(None).unwrap();
    }

    /// The guard opens for `1` and for nothing else -- not for `true`, not for
    /// `yes`. Refusing a plausible spelling is the safe direction here: the
    /// variable exists to let an isolated test environment start as root, and
    /// a typo must leave the refusal in place.
    ///
    /// The rule now lives in the declaration
    /// (`env_vars::BROKER_ALLOW_PRIVILEGED`, an `ExactValue` kind), so this
    /// asserts the behaviour the guard actually gets rather than a private
    /// copy of the comparison.
    #[test]
    fn allow_env_value_requires_exact_one() {
        assert!(crate::env_vars::BROKER_ALLOW_PRIVILEGED.kind == EnvKind::ExactValue("1"));
    }
}
