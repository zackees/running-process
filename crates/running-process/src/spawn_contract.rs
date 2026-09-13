//! Canonical resource-placement policy, independent from process lifetime.
//!
//! These value types use only the standard library. Selecting a mode does not
//! start a manager, register a task, or lazily launch a broker.

use std::{path::PathBuf, time::Duration};

/// Which resource boundary owns a newly launched process.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SpawnMode {
    /// Preserve direct spawning and inherited cgroup/Job Object placement.
    #[default]
    Inherited,
    /// Require verified placement outside the requesting worker's boundary.
    /// Enclosing user, container and machine limits still apply.
    Independent,
}

/// Handle lifetime is not resource placement or owner-death binding.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SpawnLifetime {
    /// Stop when the returned handle is dropped normally. This is not an
    /// OS-level guarantee for abrupt termination of the handle's owner.
    #[default]
    KillOnDrop,
    /// Dropping the handle leaves a successfully committed process running.
    Detached,
}

/// Explicit independent-launch authority. No implicit fallback is permitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IndependentBackend {
    /// Use a same-user native scheduler with this installed launcher binary.
    /// Availability and actual placement must be verified at launch time.
    NativeScheduler { launcher: PathBuf },
    /// Connect to an already-running broker outside the worker boundary.
    /// The launching process must never create this broker on demand.
    ExternalBroker { endpoint: String },
}

/// Canonical spawn policy. Defaults do not require any external authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnOptions {
    pub mode: SpawnMode,
    pub lifetime: SpawnLifetime,
    /// Required for Independent, absent for Inherited. Conflicting selection
    /// is an error rather than silently ignoring an explicit backend choice.
    pub backend: Option<IndependentBackend>,
    /// Combined scheduling and application-readiness budget.
    pub timeout: Duration,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            mode: SpawnMode::Inherited,
            lifetime: SpawnLifetime::KillOnDrop,
            backend: None,
            timeout: Duration::from_secs(30),
        }
    }
}
