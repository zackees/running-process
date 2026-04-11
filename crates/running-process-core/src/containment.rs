//! Process containment via OS-level mechanisms.
//!
//! `ContainedProcessGroup` ensures all child processes die when the group is
//! dropped — even on a crash.
//!
//! - **Windows**: Uses a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
//!   Dropping the group closes the handle, and Windows automatically terminates
//!   every process still assigned to the job.
//! - **Linux**: Uses `setpgid(0, 0)` to place children in a new process group
//!   and `PR_SET_PDEATHSIG(SIGKILL)` via `prctl()` so the kernel kills the
//!   child when the parent thread exits.
//!   **Caveat**: `PR_SET_PDEATHSIG` is reset on `execve` of a set-uid/set-gid
//!   binary and is tied to the *thread* that called `fork`, not the process
//!   leader. If the spawning thread exits before the parent process, children
//!   receive the signal prematurely.
//! - **macOS**: Uses `setpgid(0, 0)` for process grouping. `PR_SET_PDEATHSIG`
//!   is not available; parent-death notification is best-effort via polling
//!   `getppid()` in the child (not implemented here — the Drop-based SIGKILL
//!   to the process group is the primary mechanism).
//!
//! `Containment::Detached` spawns a process that intentionally survives the
//! group's lifetime (daemon pattern).
//!
//! # `RUNNING_PROCESS_ORIGINATOR` environment variable
//!
//! When an `originator` is set on a `ContainedProcessGroup`, all spawned child
//! processes inherit the environment variable `RUNNING_PROCESS_ORIGINATOR` with
//! the format `TOOL:PID`, where:
//!
//! - **TOOL** is the originator name (e.g., `"CLUD"`, `"JUPYTER"`)
//! - **PID** is the process ID of the parent that spawned the group
//!
//! Example value: `RUNNING_PROCESS_ORIGINATOR=CLUD:12345`
//!
//! ## Purpose
//!
//! This env var enables **cross-process session discovery** after crashes.
//!
//! ## Example
//!
//! ```no_run
//! use running_process_core::ContainedProcessGroup;
//!
//! let group = ContainedProcessGroup::with_originator("CLUD").unwrap();
//! let mut cmd = std::process::Command::new("sleep");
//! cmd.arg("60");
//! let child = group.spawn(&mut cmd).unwrap();
//! ```
//!
//! # `RUNNING_PROCESS_ORIGINATOR` environment variable
//!
//! When an `originator` is set on a `ContainedProcessGroup`, all spawned child
//! processes inherit the environment variable `RUNNING_PROCESS_ORIGINATOR` with
//! the format `TOOL:PID`, where:
//!
//! - **TOOL** is the originator name (e.g., `"CLUD"`, `"JUPYTER"`)
//! - **PID** is the process ID of the parent that spawned the group
//!
//! Example value: `RUNNING_PROCESS_ORIGINATOR=CLUD:12345`
//!
//! ## Purpose
//!
//! This env var enables **cross-process session discovery** after crashes.
//!
//! ## Example
//!
//! ```no_run
//! use running_process_core::ContainedProcessGroup;
//!
//! let group = ContainedProcessGroup::with_originator("CLUD").unwrap();
//! let mut cmd = std::process::Command::new("sleep");
//! cmd.arg("60");
//! let child = group.spawn(&mut cmd).unwrap();
//! ```

use std::process::{Child, Command};

/// The environment variable name injected into child processes for
/// cross-process session discovery.
pub const ORIGINATOR_ENV_VAR: &str = "RUNNING_PROCESS_ORIGINATOR";

/// Containment policy for a spawned process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Containment {
    /// The process is contained: it will be killed when the group is dropped,
    /// and (on Linux) when the parent thread dies.
    #[default]
    Contained,
    /// The process is detached: it will survive the group being dropped.
    /// Useful for daemon processes.
    Detached,
}

/// A group of processes that are killed together when the group is dropped.
///
/// On Windows this wraps a Job Object; on Unix it tracks a process-group ID
/// and sends `SIGKILL` to the group on drop.
pub struct ContainedProcessGroup {
    originator: Option<String>,

    #[cfg(windows)]
    job: super::WindowsJobHandle,

    #[cfg(unix)]
    pgid: std::sync::Mutex<Option<i32>>,

    #[cfg(unix)]
    child_pids: std::sync::Mutex<Vec<u32>>,
}

/// A handle to a process spawned inside a `ContainedProcessGroup`.
pub struct ContainedChild {
    pub child: Child,
    pub containment: Containment,
}

/// Format the originator env var value: `TOOL:PID`.
fn format_originator_value(tool: &str) -> String {
    format!("{}:{}", tool, std::process::id())
}

impl ContainedProcessGroup {
    /// Create a new process group without an originator.
    pub fn new() -> Result<Self, std::io::Error> {
        Self::build(None)
    }

    /// Create a new process group with an originator name.
    pub fn with_originator(originator: &str) -> Result<Self, std::io::Error> {
        Self::build(Some(originator.to_string()))
    }

    fn build(originator: Option<String>) -> Result<Self, std::io::Error> {
        #[cfg(windows)]
        {
            Self::new_windows(originator)
        }
        #[cfg(unix)]
        {
            Ok(Self {
                originator,
                pgid: std::sync::Mutex::new(None),
                child_pids: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    /// Returns the originator name, if set.
    pub fn originator(&self) -> Option<&str> {
        self.originator.as_deref()
    }

    /// Returns the full originator env var value (`TOOL:PID`), if set.
    pub fn originator_value(&self) -> Option<String> {
        self.originator.as_ref().map(|o| format_originator_value(o))
    }

    fn inject_originator_env(&self, command: &mut Command) {
        if let Some(ref originator) = self.originator {
            command.env(ORIGINATOR_ENV_VAR, format_originator_value(originator));
        }
    }

    /// Spawn a contained child process. The child will be killed when this
    /// group is dropped.
    pub fn spawn(&self, command: &mut Command) -> Result<ContainedChild, std::io::Error> {
        self.spawn_with_containment(command, Containment::Contained)
    }

    /// Spawn a detached child process. The child will survive this group
    /// being dropped.
    pub fn spawn_detached(&self, command: &mut Command) -> Result<ContainedChild, std::io::Error> {
        self.spawn_with_containment(command, Containment::Detached)
    }

    /// Spawn a child process with the given containment policy.
    pub fn spawn_with_containment(
        &self,
        command: &mut Command,
        containment: Containment,
    ) -> Result<ContainedChild, std::io::Error> {
        self.inject_originator_env(command);

        #[cfg(windows)]
        {
            self.spawn_windows(command, containment)
        }
        #[cfg(unix)]
        {
            self.spawn_unix(command, containment)
        }
    }
}

// ── Windows implementation ──────────────────────────────────────────────────

#[cfg(windows)]
impl ContainedProcessGroup {
    fn new_windows(originator: Option<String>) -> Result<Self, std::io::Error> {
        use std::mem::zeroed;
        use winapi::shared::minwindef::FALSE;
        use winapi::um::handleapi::INVALID_HANDLE_VALUE;
        use winapi::um::jobapi2::{CreateJobObjectW, SetInformationJobObject};
        use winapi::um::winnt::{
            JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
        if job.is_null() || job == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
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
            let err = std::io::Error::last_os_error();
            unsafe { winapi::um::handleapi::CloseHandle(job) };
            return Err(err);
        }

        Ok(Self {
            originator,
            job: super::WindowsJobHandle(job as usize),
        })
    }

    fn spawn_windows(
        &self,
        command: &mut Command,
        containment: Containment,
    ) -> Result<ContainedChild, std::io::Error> {
        use winapi::shared::minwindef::FALSE;
        use winapi::um::jobapi2::AssignProcessToJobObject;

        match containment {
            Containment::Contained => {
                // Spawn the child, then assign it to our Job Object.
                let child = command.spawn()?;
                let handle = {
                    use std::os::windows::io::AsRawHandle;
                    child.as_raw_handle()
                };
                let ok = unsafe {
                    AssignProcessToJobObject(
                        self.job.0 as winapi::shared::ntdef::HANDLE,
                        handle.cast(),
                    )
                };
                if ok == FALSE {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(ContainedChild { child, containment })
            }
            Containment::Detached => {
                // Detached: simply do NOT assign the child to the Job
                // Object. The child will survive when the job handle is
                // closed (and contained siblings are killed).
                //
                // NOTE: `CREATE_BREAKAWAY_FROM_JOB` is only useful when
                // the *spawning* process is already inside a job and wants
                // to launch a child outside it. Here, our spawning process
                // is not in the job, so we just skip assignment.
                let child = command.spawn()?;
                Ok(ContainedChild { child, containment })
            }
        }
    }
}

// ── Unix implementation ─────────────────────────────────────────────────────

#[cfg(unix)]
impl ContainedProcessGroup {
    fn spawn_unix(
        &self,
        command: &mut Command,
        containment: Containment,
    ) -> Result<ContainedChild, std::io::Error> {
        use std::os::unix::process::CommandExt;

        match containment {
            Containment::Contained => {
                let pgid_lock = self.pgid.lock().expect("pgid mutex poisoned");
                let target_pgid = *pgid_lock;
                drop(pgid_lock);

                unsafe {
                    command.pre_exec(move || {
                        // Place child into the group's process group, or create
                        // a new one if this is the first child.
                        let pgid = target_pgid.unwrap_or(0);
                        if libc::setpgid(0, pgid) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }

                        // Linux-only: ask the kernel to send SIGKILL to this
                        // child when the parent thread exits.
                        // NOTE: PR_SET_PDEATHSIG is tied to the calling
                        // *thread*, not the process. If the thread that spawned
                        // this child exits, the child receives the signal even
                        // if the parent process is still alive.
                        #[cfg(target_os = "linux")]
                        {
                            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                                return Err(std::io::Error::last_os_error());
                            }
                            // Re-check that the parent hasn't already died
                            // between fork() and prctl().
                            if libc::getppid() == 1 {
                                // Parent already exited; init adopted us.
                                libc::_exit(1);
                            }
                        }

                        Ok(())
                    });
                }

                let child = command.spawn()?;
                let pid = child.id();

                // Record the process group ID.
                let mut pgid_lock = self.pgid.lock().expect("pgid mutex poisoned");
                let group_pgid = if let Some(existing) = *pgid_lock {
                    existing
                } else {
                    // First child becomes the process group leader.
                    *pgid_lock = Some(pid as i32);
                    pid as i32
                };
                drop(pgid_lock);

                // Parent-side setpgid: the standard double-setpgid pattern.
                // Both parent and child call setpgid so the group assignment
                // is guaranteed regardless of scheduling order.  EACCES is
                // expected (child already exec'd) and harmless.
                unsafe {
                    libc::setpgid(pid as i32, group_pgid);
                }

                self.child_pids
                    .lock()
                    .expect("child_pids mutex poisoned")
                    .push(pid);

                Ok(ContainedChild { child, containment })
            }
            Containment::Detached => {
                unsafe {
                    command.pre_exec(|| {
                        // Create a new session so the child is fully detached.
                        if libc::setsid() == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                let child = command.spawn()?;
                Ok(ContainedChild { child, containment })
            }
        }
    }
}

#[cfg(unix)]
impl Drop for ContainedProcessGroup {
    fn drop(&mut self) {
        let pgid = self.pgid.lock().expect("pgid mutex poisoned");
        if let Some(pgid) = *pgid {
            // Send SIGKILL to the entire process group. Negative PID targets
            // the group. Errors are ignored (processes may have already exited).
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
        drop(pgid);

        // Fallback: kill each tracked PID individually, in case any child
        // failed to join the process group (e.g. race between fork and exec).
        let pids = self.child_pids.lock().expect("child_pids mutex poisoned");
        for &pid in pids.iter() {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }

        // Reap zombie children.  After SIGKILL, child processes remain as
        // zombies in the process table until waitpid() is called.  Without
        // reaping, kill(pid, 0) still reports them as alive and they consume
        // a slot in the process table.  SIGKILL is unblockable so blocking
        // waitpid returns essentially immediately.  If the PID is not our
        // child (or was already reaped), waitpid returns -1/ECHILD which we
        // safely ignore.
        for &pid in pids.iter() {
            unsafe {
                libc::waitpid(pid as i32, std::ptr::null_mut(), 0);
            }
        }
    }
}

// Windows: Job Object handle is closed by WindowsJobHandle::drop, which
// triggers JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE automatically.

// ── Default trait ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containment_default_is_contained() {
        assert_eq!(Containment::default(), Containment::Contained);
    }

    #[test]
    fn containment_clone_and_copy() {
        let c = Containment::Contained;
        let c2 = c;
        assert_eq!(c, c2);
    }

    #[test]
    fn containment_debug_format() {
        assert_eq!(format!("{:?}", Containment::Contained), "Contained");
        assert_eq!(format!("{:?}", Containment::Detached), "Detached");
    }

    #[test]
    fn contained_process_group_creates_successfully() {
        let group = ContainedProcessGroup::new();
        assert!(group.is_ok());
    }

    #[test]
    fn with_originator_creates_successfully() {
        let group = ContainedProcessGroup::with_originator("CLUD");
        assert!(group.is_ok());
        let group = group.unwrap();
        assert_eq!(group.originator(), Some("CLUD"));
    }

    #[test]
    fn originator_value_format() {
        let group = ContainedProcessGroup::with_originator("CLUD").unwrap();
        let value = group.originator_value().unwrap();
        let expected = format!("CLUD:{}", std::process::id());
        assert_eq!(value, expected);
    }

    #[test]
    fn no_originator_returns_none() {
        let group = ContainedProcessGroup::new().unwrap();
        assert!(group.originator().is_none());
        assert!(group.originator_value().is_none());
    }

    #[test]
    fn format_originator_value_correct() {
        let value = format_originator_value("JUPYTER");
        let parts: Vec<&str> = value.splitn(2, ':').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "JUPYTER");
        assert_eq!(parts[1], std::process::id().to_string());
    }
}
