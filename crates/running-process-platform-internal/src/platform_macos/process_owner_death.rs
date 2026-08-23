//! Owner-death containment for this process (macos).


use crate::platform::process::{OwnerDeathCleanup, OwnerDeathCleanupError};

/// macOS has no parent-death signal and no job objects.
///
/// Reporting that plainly is the useful answer: the caller must spawn a
/// supervisor that watches the owner and does the reaping itself. Returning
/// `Unsupported` here would say "nothing can be done", which is false and
/// would lose the containment the supervisor provides.
pub fn install_owner_death_cleanup() -> Result<OwnerDeathCleanup, OwnerDeathCleanupError> {
    Ok(OwnerDeathCleanup::SupervisorRequired)
}

/// What this host will attempt, without attempting it.
pub fn owner_death_cleanup_target() -> OwnerDeathCleanup {
    OwnerDeathCleanup::SupervisorRequired
}
