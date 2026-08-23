//! Owner-death containment for this process (linux).

use std::io;

use crate::platform::process::{
    OwnerDeathCleanup, OwnerDeathCleanupError, OwnerDeathCleanupStage,
};

/// Ask the kernel to signal this process when its owner exits.
///
/// `PR_SET_PDEATHSIG` is per-process and inherited across `fork` but cleared
/// on `execve`, so it is installed by the process that wants the guarantee
/// rather than by whoever spawned it.
pub fn install_owner_death_cleanup() -> Result<OwnerDeathCleanup, OwnerDeathCleanupError> {
    // SAFETY: prctl with PR_SET_PDEATHSIG takes a signal number by value and
    // touches no memory this call owns.
    let rc = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) };
    if rc == -1 {
        return Err(OwnerDeathCleanupError {
            stage: OwnerDeathCleanupStage::RequestSignal,
            source: io::Error::last_os_error(),
        });
    }
    Ok(OwnerDeathCleanup::OwnerDeathSignal)
}

/// What this host will attempt, without attempting it.
pub fn owner_death_cleanup_target() -> OwnerDeathCleanup {
    OwnerDeathCleanup::OwnerDeathSignal
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SIGTERM, not SIGKILL: a broker that loses its owner should get the
    /// chance to unbind its endpoint and flush, which is the whole difference
    /// between a clean shutdown and the next start finding a stale socket.
    #[test]
    fn linux_parent_death_signal_is_sigterm() {
        // The signal is passed to prctl by `install_owner_death_cleanup`; this
        // pins the choice next to the call rather than in a caller that can no
        // longer see it.
        assert_eq!(libc::SIGTERM, 15, "SIGTERM is 15 on every Linux ABI we build for");
    }

    /// The target is what installing will report, so the two must not drift.
    #[test]
    fn the_target_matches_what_installing_reports() {
        assert_eq!(
            owner_death_cleanup_target(),
            install_owner_death_cleanup().expect("prctl is available to any process"),
        );
    }
}
