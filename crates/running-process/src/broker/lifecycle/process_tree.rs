//! Process-tree cleanup setup for the broker.
//!
//! The broker can launch backend processes. Installing cleanup before
//! argument dispatch ensures later serve modes inherit the same
//! parent-death / kill-on-close containment behavior from process start.

use std::io;

/// Cleanup mechanism installed, or explicitly planned, for the current broker
/// process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessTreeCleanup {
    /// Linux `PR_SET_PDEATHSIG` was installed for the broker process.
    LinuxParentDeathSignal,
    /// Windows kill-on-job-close containment was installed.
    WindowsKillOnJobClose,
    /// Windows reported that the process already belongs to a Job Object.
    WindowsAlreadyInJob,
    /// macOS kqueue-supervisor containment is the planned Phase 5 target.
    MacosKqueueSupervisorPlanned,
    /// The current platform has no broker process-tree primitive yet.
    UnsupportedNoop,
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
/// On macOS this returns
/// [`ProcessTreeCleanup::MacosKqueueSupervisorPlanned`] to make the Phase 5
/// kqueue-supervisor contract explicit before the supervisor is implemented.
/// Other platforms currently return
/// [`ProcessTreeCleanup::UnsupportedNoop`].
pub fn install_cleanup() -> Result<ProcessTreeCleanup, ProcessTreeError> {
    platform_install_cleanup()
}

/// Return the cleanup mechanism this platform attempts to install.
pub fn cleanup_target() -> ProcessTreeCleanup {
    cleanup_target_for_platform(current_platform())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupPlatform {
    #[cfg(any(target_os = "linux", test))]
    Linux,
    #[cfg(any(windows, test))]
    Windows,
    #[cfg(any(target_os = "macos", test))]
    Macos,
    #[cfg(any(
        all(unix, not(any(target_os = "linux", target_os = "macos"))),
        all(not(unix), not(windows)),
        test
    ))]
    Other,
}

fn cleanup_target_for_platform(platform: CleanupPlatform) -> ProcessTreeCleanup {
    match platform {
        #[cfg(any(target_os = "linux", test))]
        CleanupPlatform::Linux => ProcessTreeCleanup::LinuxParentDeathSignal,
        #[cfg(any(windows, test))]
        CleanupPlatform::Windows => ProcessTreeCleanup::WindowsKillOnJobClose,
        #[cfg(any(target_os = "macos", test))]
        CleanupPlatform::Macos => ProcessTreeCleanup::MacosKqueueSupervisorPlanned,
        #[cfg(any(
            all(unix, not(any(target_os = "linux", target_os = "macos"))),
            all(not(unix), not(windows)),
            test
        ))]
        CleanupPlatform::Other => ProcessTreeCleanup::UnsupportedNoop,
    }
}

#[cfg(target_os = "linux")]
fn current_platform() -> CleanupPlatform {
    CleanupPlatform::Linux
}

#[cfg(windows)]
fn current_platform() -> CleanupPlatform {
    CleanupPlatform::Windows
}

#[cfg(target_os = "macos")]
fn current_platform() -> CleanupPlatform {
    CleanupPlatform::Macos
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn current_platform() -> CleanupPlatform {
    CleanupPlatform::Other
}

#[cfg(all(not(unix), not(windows)))]
fn current_platform() -> CleanupPlatform {
    CleanupPlatform::Other
}

#[cfg(target_os = "linux")]
fn platform_install_cleanup() -> Result<ProcessTreeCleanup, ProcessTreeError> {
    let rc = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, linux_parent_death_signal()) };
    if rc == -1 {
        Err(ProcessTreeError::LinuxParentDeathSignal(
            io::Error::last_os_error(),
        ))
    } else {
        Ok(ProcessTreeCleanup::LinuxParentDeathSignal)
    }
}

#[cfg(target_os = "linux")]
fn linux_parent_death_signal() -> libc::c_int {
    libc::SIGTERM
}

#[cfg(windows)]
fn platform_install_cleanup() -> Result<ProcessTreeCleanup, ProcessTreeError> {
    if JOB_HANDLE.get().is_some() {
        return Ok(ProcessTreeCleanup::WindowsKillOnJobClose);
    }

    let job = create_kill_on_close_job()?;
    match assign_current_process_to_job(job.as_raw()) {
        Ok(()) => match JOB_HANDLE.set(job) {
            Ok(()) => Ok(ProcessTreeCleanup::WindowsKillOnJobClose),
            Err(job) => {
                // Avoid closing a job handle that may contain the current
                // process. Leaking the duplicate setup handle is preferable
                // to terminating the broker in an impossible double-install
                // race.
                std::mem::forget(job);
                Ok(ProcessTreeCleanup::WindowsAlreadyInJob)
            }
        },
        Err(source) if windows_error_is_access_denied(&source) => {
            Ok(ProcessTreeCleanup::WindowsAlreadyInJob)
        }
        Err(source) => Err(ProcessTreeError::WindowsJobAssign(source)),
    }
}

#[cfg(target_os = "macos")]
fn platform_install_cleanup() -> Result<ProcessTreeCleanup, ProcessTreeError> {
    Ok(ProcessTreeCleanup::MacosKqueueSupervisorPlanned)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn platform_install_cleanup() -> Result<ProcessTreeCleanup, ProcessTreeError> {
    Ok(ProcessTreeCleanup::UnsupportedNoop)
}

#[cfg(all(not(unix), not(windows)))]
fn platform_install_cleanup() -> Result<ProcessTreeCleanup, ProcessTreeError> {
    Ok(ProcessTreeCleanup::UnsupportedNoop)
}

#[cfg(windows)]
static JOB_HANDLE: std::sync::OnceLock<WindowsJobHandle> = std::sync::OnceLock::new();

#[cfg(windows)]
struct WindowsJobHandle(usize);

#[cfg(windows)]
impl WindowsJobHandle {
    fn as_raw(&self) -> winapi::um::winnt::HANDLE {
        self.0 as winapi::um::winnt::HANDLE
    }
}

#[cfg(windows)]
impl Drop for WindowsJobHandle {
    fn drop(&mut self) {
        unsafe {
            winapi::um::handleapi::CloseHandle(self.as_raw());
        }
    }
}

#[cfg(windows)]
fn create_kill_on_close_job() -> Result<WindowsJobHandle, ProcessTreeError> {
    use winapi::shared::minwindef::FALSE;
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::jobapi2::{CreateJobObjectW, SetInformationJobObject};
    use winapi::um::winnt::{
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
    if job.is_null() || job == INVALID_HANDLE_VALUE {
        return Err(ProcessTreeError::WindowsJobCreate(
            io::Error::last_os_error(),
        ));
    }

    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == FALSE {
        let err = io::Error::last_os_error();
        unsafe { CloseHandle(job) };
        return Err(ProcessTreeError::WindowsJobCreate(err));
    }

    Ok(WindowsJobHandle(job as usize))
}

#[cfg(windows)]
fn assign_current_process_to_job(job: winapi::um::winnt::HANDLE) -> Result<(), io::Error> {
    use winapi::shared::minwindef::FALSE;
    use winapi::um::jobapi2::AssignProcessToJobObject;
    use winapi::um::processthreadsapi::GetCurrentProcess;

    let ok = unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) };
    if ok == FALSE {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn windows_error_is_access_denied(err: &io::Error) -> bool {
    use winapi::shared::winerror::ERROR_ACCESS_DENIED;

    err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_target_model_states_phase_5_platform_contracts() {
        assert_eq!(
            cleanup_target_for_platform(CleanupPlatform::Linux),
            ProcessTreeCleanup::LinuxParentDeathSignal
        );
        assert_eq!(
            cleanup_target_for_platform(CleanupPlatform::Windows),
            ProcessTreeCleanup::WindowsKillOnJobClose
        );
        assert_eq!(
            cleanup_target_for_platform(CleanupPlatform::Macos),
            ProcessTreeCleanup::MacosKqueueSupervisorPlanned
        );
        assert_eq!(
            cleanup_target_for_platform(CleanupPlatform::Other),
            ProcessTreeCleanup::UnsupportedNoop
        );
    }

    #[test]
    fn cleanup_target_is_explicit_for_current_platform() {
        #[cfg(target_os = "linux")]
        assert_eq!(cleanup_target(), ProcessTreeCleanup::LinuxParentDeathSignal);

        #[cfg(windows)]
        assert_eq!(cleanup_target(), ProcessTreeCleanup::WindowsKillOnJobClose);

        #[cfg(target_os = "macos")]
        assert_eq!(
            cleanup_target(),
            ProcessTreeCleanup::MacosKqueueSupervisorPlanned
        );

        #[cfg(all(not(any(target_os = "linux", target_os = "macos")), not(windows)))]
        assert_eq!(cleanup_target(), ProcessTreeCleanup::UnsupportedNoop);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_parent_death_signal_is_sigterm() {
        assert_eq!(linux_parent_death_signal(), libc::SIGTERM);
    }
}
