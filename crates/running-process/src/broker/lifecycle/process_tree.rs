//! Process-tree cleanup setup for the broker.
//!
//! The broker can launch backend processes. Installing cleanup before
//! argument dispatch ensures later serve modes inherit the same
//! parent-death / kill-on-close containment behavior from process start.

use std::{io, time::Duration};

use crate::platform::process::{OwnerDeathCleanup, OwnerDeathCleanupError, OwnerDeathCleanupStage};

/// Cleanup mechanism installed, or concrete lifecycle contract selected, for
/// the current broker process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessTreeCleanup {
    /// Linux `PR_SET_PDEATHSIG` was installed for the broker process.
    LinuxParentDeathSignal,
    /// Windows kill-on-job-close containment was installed.
    WindowsKillOnJobClose,
    /// Windows reported that the process already belongs to a Job Object.
    WindowsAlreadyInJob,
    /// macOS kqueue-supervisor containment is the Phase 5 contract.
    MacosKqueueSupervisorContract,
    /// The current platform has no broker process-tree primitive yet.
    UnsupportedNoop,
}

/// Maximum Phase 5 cleanup budget for a macOS backend after broker exit.
pub const MACOS_SUPERVISOR_KILL_DEADLINE: Duration = Duration::from_secs(5);

/// Concrete macOS supervisor contract for Phase 5 process-tree cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacosSupervisorContract {
    /// PID that the supervisor child watches.
    pub watch_pid: MacosSupervisorWatchPid,
    /// kqueue filter registered by the supervisor.
    pub kqueue_filter: MacosKqueueFilter,
    /// kqueue note that reports broker exit.
    pub kqueue_note: MacosKqueueNote,
    /// Startup barrier before the backend endpoint can be published.
    pub registration_barrier: MacosSupervisorRegistrationBarrier,
    /// Race guard after kqueue registration.
    pub race_guard: MacosSupervisorRaceGuard,
    /// Action the supervisor performs after observing broker exit.
    pub exit_action: MacosSupervisorExitAction,
    /// Required cleanup deadline after broker exit.
    pub kill_deadline: Duration,
}

impl MacosSupervisorContract {
    /// Return the Phase 5 macOS supervisor contract.
    pub const fn phase5() -> Self {
        Self {
            watch_pid: MacosSupervisorWatchPid::BrokerParent,
            kqueue_filter: MacosKqueueFilter::Process,
            kqueue_note: MacosKqueueNote::Exit,
            registration_barrier: MacosSupervisorRegistrationBarrier::BeforeBackendPipePublication,
            race_guard: MacosSupervisorRaceGuard::RecheckBrokerAliveAfterRegistration,
            exit_action: MacosSupervisorExitAction::SigkillBackend,
            kill_deadline: MACOS_SUPERVISOR_KILL_DEADLINE,
        }
    }

    /// Return the kqueue filter syscall name.
    pub const fn kqueue_filter_name(&self) -> &'static str {
        match self.kqueue_filter {
            MacosKqueueFilter::Process => "EVFILT_PROC",
        }
    }

    /// Return the kqueue note syscall name.
    pub const fn kqueue_note_name(&self) -> &'static str {
        match self.kqueue_note {
            MacosKqueueNote::Exit => "NOTE_EXIT",
        }
    }

    /// Return the supervisor termination signal name.
    pub const fn termination_signal_name(&self) -> &'static str {
        match self.exit_action {
            MacosSupervisorExitAction::SigkillBackend => "SIGKILL",
        }
    }
}

/// PID watched by the macOS supervisor child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacosSupervisorWatchPid {
    /// Watch the broker parent process.
    BrokerParent,
}

/// kqueue filter used by the macOS supervisor child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacosKqueueFilter {
    /// `EVFILT_PROC`.
    Process,
}

/// kqueue process note used by the macOS supervisor child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacosKqueueNote {
    /// `NOTE_EXIT`.
    Exit,
}

/// Required startup barrier for the macOS supervisor child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacosSupervisorRegistrationBarrier {
    /// Register kqueue before the backend pipe is published.
    BeforeBackendPipePublication,
}

/// Required startup race guard for the macOS supervisor child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacosSupervisorRaceGuard {
    /// Re-check that the broker is alive after kqueue registration.
    RecheckBrokerAliveAfterRegistration,
}

/// Action performed by the macOS supervisor child after broker exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacosSupervisorExitAction {
    /// Send `SIGKILL` to the backend process.
    SigkillBackend,
}

/// Return the concrete macOS kqueue-supervisor contract for Phase 5.
pub const fn macos_supervisor_contract() -> MacosSupervisorContract {
    MacosSupervisorContract::phase5()
}

/// Errors returned while installing process-tree cleanup.
#[derive(Debug, thiserror::Error)]
pub enum ProcessTreeError {
    /// Linux `prctl(PR_SET_PDEATHSIG, ...)` failed.
    #[error("failed to install Linux parent-death signal: {0}")]
    LinuxParentDeathSignal(io::Error),
    /// Windows could not create or configure a kill-on-close job.
    #[error("failed to create Windows kill-on-close Job Object: {0}")]
    WindowsJobCreate(io::Error),
    /// Windows could not assign the broker process to the job.
    #[error("failed to assign broker process to Windows Job Object: {0}")]
    WindowsJobAssign(io::Error),
}

/// Install process-tree cleanup for the current broker process.
///
/// On Linux this sets `PR_SET_PDEATHSIG` to `SIGTERM`. On Windows this assigns
/// the broker to a kill-on-close Job Object unless it already belongs to one.
/// On macOS this selects
/// [`ProcessTreeCleanup::MacosKqueueSupervisorContract`] and the concrete
/// [`MacosSupervisorContract`] that backend spawn wiring must honor before
/// publishing a backend pipe.
/// Other platforms currently return
/// [`ProcessTreeCleanup::UnsupportedNoop`].
pub fn install_cleanup() -> Result<ProcessTreeCleanup, ProcessTreeError> {
    crate::platform::process::install_owner_death_cleanup()
        .map(from_facade)
        .map_err(from_facade_error)
}

/// Return the cleanup mechanism this platform attempts to install.
pub fn cleanup_target() -> ProcessTreeCleanup {
    from_facade(crate::platform::process::owner_death_cleanup_target())
}

/// Read this host's answer and say it in the broker's own vocabulary.
///
/// The facade reports the *guarantee* it installed; this maps that onto the
/// names this module's public API has always used. The mapping is not purely
/// mechanical: `SupervisorRequired` becomes the macOS kqueue contract,
/// because on that host "the kernel will not reap for you" is precisely what
/// obliges the supervisor the contract describes.
fn from_facade(cleanup: OwnerDeathCleanup) -> ProcessTreeCleanup {
    match cleanup {
        OwnerDeathCleanup::OwnerDeathSignal => ProcessTreeCleanup::LinuxParentDeathSignal,
        OwnerDeathCleanup::KillOnOwnerHandleClose => ProcessTreeCleanup::WindowsKillOnJobClose,
        OwnerDeathCleanup::AlreadyContained => ProcessTreeCleanup::WindowsAlreadyInJob,
        OwnerDeathCleanup::SupervisorRequired => ProcessTreeCleanup::MacosKqueueSupervisorContract,
        OwnerDeathCleanup::Unsupported => ProcessTreeCleanup::UnsupportedNoop,
    }
}

/// Map a facade failure onto the variant this module has always reported.
///
/// The stage travels with the error precisely so this is a lookup rather than
/// a guess: "could not build the container" and "built it, could not join it"
/// are different situations for an operator, and these messages have
/// distinguished them since #427.
fn from_facade_error(error: OwnerDeathCleanupError) -> ProcessTreeError {
    match error.stage {
        OwnerDeathCleanupStage::RequestSignal => {
            ProcessTreeError::LinuxParentDeathSignal(error.source)
        }
        OwnerDeathCleanupStage::CreateContainer => ProcessTreeError::WindowsJobCreate(error.source),
        OwnerDeathCleanupStage::JoinContainer => ProcessTreeError::WindowsJobAssign(error.source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every guarantee a host can report maps to a stated contract.
    ///
    /// This used to walk a private `CleanupPlatform` enum that restated the
    /// mapping it was checking, so it could only ever agree with itself. It
    /// now walks the facade's own variants: if a host learns to report
    /// something new, this stops compiling until someone decides what the
    /// broker should call it.
    #[test]
    fn cleanup_target_model_states_phase_5_platform_contracts() {
        for (reported, expected) in [
            (
                OwnerDeathCleanup::OwnerDeathSignal,
                ProcessTreeCleanup::LinuxParentDeathSignal,
            ),
            (
                OwnerDeathCleanup::KillOnOwnerHandleClose,
                ProcessTreeCleanup::WindowsKillOnJobClose,
            ),
            (
                OwnerDeathCleanup::AlreadyContained,
                ProcessTreeCleanup::WindowsAlreadyInJob,
            ),
            (
                OwnerDeathCleanup::SupervisorRequired,
                ProcessTreeCleanup::MacosKqueueSupervisorContract,
            ),
            (
                OwnerDeathCleanup::Unsupported,
                ProcessTreeCleanup::UnsupportedNoop,
            ),
        ] {
            assert_eq!(from_facade(reported), expected, "{reported:?}");
        }
    }

    /// A failure keeps the step it failed at.
    ///
    /// The three variants have distinguished "could not build the container"
    /// from "built it, could not join it" since #427, and an operator reads
    /// them differently. Routing through the facade must not flatten that.
    #[test]
    fn a_failure_keeps_the_step_it_failed_at() {
        use crate::platform::process::OwnerDeathCleanupStage;

        let staged = |stage| OwnerDeathCleanupError {
            stage,
            source: io::Error::from_raw_os_error(5),
        };
        assert!(matches!(
            from_facade_error(staged(OwnerDeathCleanupStage::RequestSignal)),
            ProcessTreeError::LinuxParentDeathSignal(_)
        ));
        assert!(matches!(
            from_facade_error(staged(OwnerDeathCleanupStage::CreateContainer)),
            ProcessTreeError::WindowsJobCreate(_)
        ));
        assert!(matches!(
            from_facade_error(staged(OwnerDeathCleanupStage::JoinContainer)),
            ProcessTreeError::WindowsJobAssign(_)
        ));
    }

    /// Whatever this host is, it names a contract.
    ///
    /// This used to spell out the expected answer per host, which duplicated
    /// what the platform trees now assert next to the code that produces it.
    /// The claim worth making *here* is the one a caller depends on: every
    /// host the broker ships for has a stated containment story, so a caller
    /// deciding whether to spawn a supervisor always gets an answer.
    #[test]
    fn cleanup_target_is_explicit_for_current_platform() {
        let target = cleanup_target();
        assert_ne!(
            target,
            ProcessTreeCleanup::UnsupportedNoop,
            "every shipped host names a contract; UnsupportedNoop means one was not taught"
        );
        assert_eq!(
            target,
            install_cleanup().expect("installing must succeed where a target is claimed"),
            "the target must be what installing actually reports"
        );
    }

    #[test]
    fn macos_supervisor_contract_pins_phase_5_cleanup_requirements() {
        let contract = macos_supervisor_contract();

        assert_eq!(contract.watch_pid, MacosSupervisorWatchPid::BrokerParent);
        assert_eq!(contract.kqueue_filter_name(), "EVFILT_PROC");
        assert_eq!(contract.kqueue_note_name(), "NOTE_EXIT");
        assert_eq!(
            contract.registration_barrier,
            MacosSupervisorRegistrationBarrier::BeforeBackendPipePublication
        );
        assert_eq!(
            contract.race_guard,
            MacosSupervisorRaceGuard::RecheckBrokerAliveAfterRegistration
        );
        assert_eq!(contract.termination_signal_name(), "SIGKILL");
        assert_eq!(contract.kill_deadline, Duration::from_secs(5));
    }
}
