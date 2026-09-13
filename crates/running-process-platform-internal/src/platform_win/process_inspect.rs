//! Asking this host about another process (Windows).

use std::io;
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};

use crate::platform::process::{ProcessInspectError, ProcessInspectErrorKind};

/// `GetExitCodeProcess` reports this while a process is still running.
///
/// It is also a perfectly legal exit code, so a process that exits with 259
/// is indistinguishable from a running one by this call alone. Holding the
/// handle open is what makes that harmless: the PID cannot be reused while a
/// handle to it exists, so the wrong process is never described.
const STILL_ACTIVE: u32 = 259;

/// A live reference to another process, good for as long as it is held.
///
/// The open handle is the identity. Windows will not reissue a PID while any
/// handle to that process remains open, so a handle taken once keeps naming
/// the same process -- including after it exits, when it becomes a handle to
/// a known-dead process rather than a stale number.
pub struct ProcessLiveness {
    pid: u32,
    handle: HANDLE,
    pinned_control: bool,
}

// SAFETY: a process handle is a kernel object usable from any thread; the
// value is opaque here and never dereferenced.
unsafe impl Send for ProcessLiveness {}
unsafe impl Sync for ProcessLiveness {}

impl std::fmt::Debug for ProcessLiveness {
    /// Names the process, not the handle.
    ///
    /// The underlying descriptor or handle value is an artefact of this
    /// process's own table; printing it invites a reader to compare two
    /// numbers that were never comparable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessLiveness")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl ProcessLiveness {
    /// Acquire query, wait, and termination rights for identity-safe control.
    pub fn open_for_control(pid: u32) -> Result<Self, ProcessInspectError> {
        Self::open_pinned(pid).map_err(|source| ProcessInspectError {
            kind: ProcessInspectErrorKind::Host,
            source,
        })
    }

    /// Force termination through the held process handle, never by PID lookup.
    pub fn force_kill(&self) -> io::Result<()> {
        self.terminate_pinned()
    }

    #[cfg(all(test, feature = "independent-spawn"))]
    pub(crate) fn test_creation_time(&self) -> io::Result<u64> {
        use windows_sys::Win32::{Foundation::FILETIME, System::Threading::GetProcessTimes};
        let mut times = [FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        }; 4];
        // SAFETY: the pinned process handle is live and all four output records
        // are disjoint writable FILETIMEs for the duration of the call.
        let result = unsafe {
            let ptr = times.as_mut_ptr();
            GetProcessTimes(self.handle, ptr, ptr.add(1), ptr.add(2), ptr.add(3))
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((u64::from(times[0].dwHighDateTime) << 32) | u64::from(times[0].dwLowDateTime))
    }

    #[cfg(feature = "independent-spawn")]
    pub(crate) fn open_pinned(pid: u32) -> io::Result<Self> {
        use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;
        if pid == 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        // SAFETY: requested rights are limited to observation and termination;
        // the newly returned handle is checked and owned by this value.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            pid,
            handle,
            pinned_control: true,
        })
    }

    #[cfg(feature = "independent-spawn")]
    pub(crate) fn terminate_pinned(&self) -> io::Result<()> {
        if !self.pinned_control {
            return Err(io::Error::from(io::ErrorKind::Unsupported));
        }
        if !self.is_alive() {
            return Ok(());
        }
        // SAFETY: this value owns a live PROCESS_TERMINATE handle, not a PID.
        if unsafe { TerminateProcess(self.handle, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Query the caller's immediate job and all nested child jobs. A different
    /// scheduler-owned job is allowed; sharing the caller's worker job is not.
    #[cfg(feature = "independent-spawn")]
    pub(crate) fn outside_current_job(&self, deadline: std::time::Instant) -> io::Result<bool> {
        use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
        use windows_sys::Win32::System::{
            JobObjects::{
                IsProcessInJob, JobObjectBasicProcessIdList, QueryInformationJobObject,
                JOBOBJECT_BASIC_PROCESS_ID_LIST,
            },
            Threading::GetCurrentProcess,
        };
        let mut in_job = 0;
        // SAFETY: current-process pseudo-handle and initialized BOOL output.
        if unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if !self.is_alive() {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        if in_job == 0 {
            return Ok(true);
        }
        let offset = std::mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList);
        let mut capacity = 128_usize;
        loop {
            if std::time::Instant::now() >= deadline {
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            if !self.is_alive() {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            let bytes = offset + capacity * std::mem::size_of::<usize>();
            let mut storage = vec![0_usize; bytes.div_ceil(std::mem::size_of::<usize>())];
            // SAFETY: usize storage provides native alignment and bytes of
            // initialized backing memory for the variable-sized PID list.
            let ok = unsafe {
                QueryInformationJobObject(
                    std::ptr::null_mut(),
                    JobObjectBasicProcessIdList,
                    storage.as_mut_ptr().cast(),
                    bytes as u32,
                    std::ptr::null_mut(),
                )
            };
            let error = io::Error::last_os_error();
            if ok == 0 && error.raw_os_error() != Some(ERROR_MORE_DATA as i32) {
                return Err(error);
            }
            // SAFETY: allocation is at least the size of the header plus one PID.
            let header = unsafe { &*storage.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() };
            let count = header.NumberOfProcessIdsInList as usize;
            if ok != 0 && header.NumberOfAssignedProcesses as usize == count && count <= capacity {
                // SAFETY: count was bounded against allocated trailing storage.
                let ids = unsafe {
                    std::slice::from_raw_parts(
                        storage.as_ptr().cast::<u8>().add(offset).cast::<usize>(),
                        count,
                    )
                };
                if !ids.contains(&(std::process::id() as usize)) {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "caller job changed during placement verification",
                    ));
                }
                if !self.is_alive() {
                    return Err(io::Error::from(io::ErrorKind::NotFound));
                }
                return Ok(!ids.contains(&(self.pid as usize)));
            }
            if capacity >= 65536 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "caller job process list exceeds verification bound",
                ));
            }
            capacity *= 2;
        }
    }

    /// Take a reference to `pid`, failing if no such process is running.
    pub fn open(pid: u32) -> Result<Self, ProcessInspectError> {
        // SAFETY: the call takes access flags, an inherit flag, and a PID by
        // value; the returned handle is checked before use.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return Err(ProcessInspectError::stated(
                ProcessInspectErrorKind::NotFound,
                "no such process",
            ));
        }
        Ok(Self {
            pid,
            handle,
            pinned_control: false,
        })
    }

    /// The process ID this handle was opened for.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether that process is still running.
    pub fn is_alive(&self) -> bool {
        if self.pinned_control {
            // SAFETY: strict handles include SYNCHRONIZE. Unlike exit-code
            // polling this correctly recognizes a process that exited with 259.
            return unsafe {
                windows_sys::Win32::System::Threading::WaitForSingleObject(self.handle, 0)
            } == windows_sys::Win32::Foundation::WAIT_TIMEOUT;
        }
        let mut exit_code = 0_u32;
        // SAFETY: `self.handle` is live for this handle's lifetime and the
        // out-parameter is a valid initialised u32.
        let ok = unsafe { GetExitCodeProcess(self.handle, &mut exit_code) };
        ok != 0 && exit_code == STILL_ACTIVE
    }
}

impl Drop for ProcessLiveness {
    fn drop(&mut self) {
        // SAFETY: `self.handle` came from OpenProcess and is closed once.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// Resolve the on-disk image a running process was started from.
pub fn process_executable_path(pid: u32) -> Result<PathBuf, io::Error> {
    // SAFETY: see `ProcessLiveness::open`.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }

    let mut path = vec![0_u16; 32768];
    let mut len = path.len() as u32;
    // SAFETY: `path` is valid for `len` wide characters, and `len` is updated
    // in place to the number actually written.
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut len) };
    let source = io::Error::last_os_error();
    // SAFETY: the handle came from OpenProcess above and is closed once.
    unsafe {
        CloseHandle(handle);
    }
    if ok == 0 {
        return Err(source);
    }

    path.truncate(len as usize);
    Ok(PathBuf::from(String::from_utf16_lossy(&path)))
}

/// Ask a process to stop.
///
/// This host has no signal that asks. Terminating without asking is a
/// different operation with different consequences for the target, so it is
/// reported as unsupported rather than quietly substituted.
pub fn process_signal_terminate(_pid: u32) -> Result<(), ProcessInspectError> {
    Err(ProcessInspectError::stated(
        ProcessInspectErrorKind::Unsupported,
        "this host has no graceful terminate signal",
    ))
}

/// Stop a process without asking.
pub fn process_force_kill(pid: u32) -> Result<(), ProcessInspectError> {
    // SAFETY: see `ProcessLiveness::open`.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        return Err(ProcessInspectError::stated(
            ProcessInspectErrorKind::NotFound,
            "no such process",
        ));
    }
    // SAFETY: `handle` is live and was opened with PROCESS_TERMINATE.
    let ok = unsafe { TerminateProcess(handle, 1) };
    let source = io::Error::last_os_error();
    // SAFETY: the handle came from OpenProcess above and is closed once.
    unsafe {
        CloseHandle(handle);
    }
    if ok == 0 {
        Err(ProcessInspectError {
            kind: ProcessInspectErrorKind::Host,
            source,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PID zero names no process this host will open.
    #[test]
    fn pid_zero_is_never_valid() {
        let error = ProcessLiveness::open(0).expect_err("pid 0");
        assert_eq!(error.kind, ProcessInspectErrorKind::NotFound);
    }

    /// This process is alive, and knows where it was started from.
    #[test]
    fn this_process_is_alive_and_locatable() {
        let me = std::process::id();
        let handle = ProcessLiveness::open(me).expect("open self");
        assert_eq!(handle.pid(), me);
        assert!(handle.is_alive());
        assert_eq!(
            process_executable_path(me).expect("exe"),
            std::env::current_exe().expect("current_exe")
        );
    }

    /// A handle keeps naming the process it was opened for, and reports it
    /// dead once it exits rather than failing to find it.
    #[test]
    fn a_dead_process_reports_dead() {
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "exit 0"])
            .spawn()
            .expect("spawn");
        let handle = ProcessLiveness::open(child.id()).expect("open child");
        child.wait().expect("wait");
        assert!(!handle.is_alive(), "an exited child must report dead");
    }

    /// Asking politely is not silently upgraded to terminating.
    #[test]
    fn graceful_terminate_is_reported_unsupported() {
        let error = process_signal_terminate(std::process::id()).expect_err("unsupported");
        assert_eq!(error.kind, ProcessInspectErrorKind::Unsupported);
    }
}

/// Whether two spellings name the same executable image on this host.
///
/// This host's paths are case-insensitive, and it reports long paths with a
/// `\\?\` prefix that the same file is equally reachable without. Comparing
/// the two spellings literally would call one image two different files.
///
/// Both sides are canonicalised first where the file is reachable; a path
/// that cannot be canonicalised is compared as written rather than treated as
/// a mismatch, because "the file moved" and "the caller lacks permission to
/// resolve it" arrive here identically.
pub fn process_same_executable_path(actual: &std::path::Path, expected: &std::path::Path) -> bool {
    comparable(actual) == comparable(expected)
}

fn comparable(path: &std::path::Path) -> String {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let path = path.to_string_lossy().replace('\\', "/");
    let path = path.strip_prefix("//?/").unwrap_or(&path);
    path.to_ascii_lowercase()
}

#[cfg(test)]
mod path_tests {
    use super::*;
    use std::path::Path;

    /// Case and the verbatim prefix are spelling, not identity.
    #[test]
    fn spelling_differences_do_not_make_two_images() {
        assert!(process_same_executable_path(
            Path::new(r"C:\Windows\System32\cmd.exe"),
            Path::new(r"c:\windows\system32\CMD.EXE"),
        ));
        assert!(process_same_executable_path(
            Path::new(r"\\?\C:\tmp\daemon.exe"),
            Path::new(r"C:\tmp\daemon.exe"),
        ));
    }

    /// Two genuinely different images still compare different.
    #[test]
    fn different_images_are_still_different() {
        assert!(!process_same_executable_path(
            Path::new(r"C:\tmp\daemon.exe"),
            Path::new(r"C:\tmp\other.exe"),
        ));
    }
}
