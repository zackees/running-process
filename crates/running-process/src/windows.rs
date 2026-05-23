#![cfg(windows)]

use std::process::Child;

pub(crate) struct WindowsJobHandle(pub(crate) usize);

impl Drop for WindowsJobHandle {
    fn drop(&mut self) {
        unsafe {
            winapi::um::handleapi::CloseHandle(self.0 as winapi::shared::ntdef::HANDLE);
        }
    }
}

/// Parent-side handles for the captured stdout/stderr pipes, kept so
/// that `kill_impl` can call `CancelIoEx` to interrupt a reader thread
/// blocked in `read()`. Stored as `usize` because `RawHandle` (a raw
/// pointer) is not `Send` and we share this via `Arc<Mutex<...>>`.
///
/// The reader thread clears its slot (under the mutex) immediately
/// before dropping its `ChildStd*`, so `kill_impl` never calls
/// `CancelIoEx` on a closed (and potentially reused) handle.
#[derive(Default)]
pub(crate) struct CapturePipeHandles {
    pub(crate) stdout: Option<usize>,
    pub(crate) stderr: Option<usize>,
}

pub(crate) fn assign_child_to_windows_kill_on_close_job_impl(
    child: &Child,
) -> Result<WindowsJobHandle, std::io::Error> {
    crate::rp_rust_debug_scope!("running_process::assign_child_to_windows_kill_on_close_job");
    use std::mem::zeroed;
    use std::os::windows::io::AsRawHandle;

    use winapi::shared::minwindef::FALSE;
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::jobapi2::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    };
    use winapi::um::winnt::{
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    let handle = child.as_raw_handle();
    let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
    if job.is_null() || job == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }

    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
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
        unsafe { CloseHandle(job) };
        return Err(err);
    }

    let ok = unsafe { AssignProcessToJobObject(job, handle.cast()) };
    if ok == FALSE {
        let err = std::io::Error::last_os_error();
        unsafe { CloseHandle(job) };
        return Err(err);
    }

    Ok(WindowsJobHandle(job as usize))
}

pub(crate) fn windows_priority_flags(nice: Option<i32>) -> u32 {
    const IDLE_PRIORITY_CLASS: u32 = 0x0000_0040;
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
    const ABOVE_NORMAL_PRIORITY_CLASS: u32 = 0x0000_8000;
    const HIGH_PRIORITY_CLASS: u32 = 0x0000_0080;

    match nice {
        Some(value) if value >= 15 => IDLE_PRIORITY_CLASS,
        Some(value) if value >= 1 => BELOW_NORMAL_PRIORITY_CLASS,
        Some(value) if value <= -15 => HIGH_PRIORITY_CLASS,
        Some(value) if value <= -1 => ABOVE_NORMAL_PRIORITY_CLASS,
        _ => 0,
    }
}
