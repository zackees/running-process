//! Windows Job Object containment and descendant lifecycle delivery.

use std::process::Child;

use crate::platform::process::DescendantEvent;

/// Owns the kill-on-close Job Object and its optional completion port.
pub struct WindowsJobHandle {
    job: usize,
    iocp: Option<usize>,
}

impl Drop for WindowsJobHandle {
    fn drop(&mut self) {
        unsafe {
            winapi::um::handleapi::CloseHandle(self.job as winapi::shared::ntdef::HANDLE);
            if let Some(port) = self.iocp.take() {
                winapi::um::handleapi::CloseHandle(port as winapi::shared::ntdef::HANDLE);
            }
        }
    }
}

/// Put `child` in a kill-on-close Job Object and, when requested, attach the
/// Job to an IOCP before assignment so no initial notification can race past.
pub fn assign_child_to_windows_job(
    child: &Child,
    direct_pid: u32,
    address_space_limit_bytes: Option<u64>,
    emit: Option<Box<dyn Fn(DescendantEvent) + Send>>,
) -> Result<WindowsJobHandle, std::io::Error> {
    use std::mem::zeroed;
    use winapi::shared::minwindef::FALSE;
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::jobapi2::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    };
    use winapi::um::winnt::{
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };

    let handle = super::sync_child_native_handle(child);
    let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
    if job.is_null() || job == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }

    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    let mut limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    if address_space_limit_bytes.is_some() {
        limit_flags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
    }
    info.BasicLimitInformation.LimitFlags = limit_flags;
    if let Some(limit) = address_space_limit_bytes {
        #[allow(clippy::cast_possible_truncation)]
        {
            info.ProcessMemoryLimit = limit as usize;
        }
    }
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw mut info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == FALSE {
        let error = std::io::Error::last_os_error();
        unsafe { CloseHandle(job) };
        return Err(error);
    }

    let iocp = match emit {
        Some(emit) => match attach_iocp_pump(job, emit, direct_pid) {
            Ok(port) => Some(port),
            Err(error) => {
                unsafe { CloseHandle(job) };
                return Err(error);
            }
        },
        None => None,
    };

    if unsafe { AssignProcessToJobObject(job, handle as _) } == FALSE {
        let error = std::io::Error::last_os_error();
        unsafe { CloseHandle(job) };
        if let Some(port) = iocp {
            unsafe { CloseHandle(port as winapi::shared::ntdef::HANDLE) };
        }
        return Err(error);
    }

    Ok(WindowsJobHandle {
        job: job as usize,
        iocp,
    })
}

fn attach_iocp_pump(
    job: winapi::shared::ntdef::HANDLE,
    emit: Box<dyn Fn(DescendantEvent) + Send>,
    direct_pid: u32,
) -> Result<usize, std::io::Error> {
    use std::mem::zeroed;
    use winapi::shared::minwindef::FALSE;
    use winapi::um::handleapi::INVALID_HANDLE_VALUE;
    use winapi::um::ioapiset::CreateIoCompletionPort;
    use winapi::um::jobapi2::SetInformationJobObject;
    use winapi::um::winnt::{
        JobObjectAssociateCompletionPortInformation, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
    };

    let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
    if port.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let mut assoc: JOBOBJECT_ASSOCIATE_COMPLETION_PORT = unsafe { zeroed() };
    assoc.CompletionKey = job.cast();
    assoc.CompletionPort = port;
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectAssociateCompletionPortInformation,
            (&raw mut assoc).cast(),
            std::mem::size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
        )
    };
    if ok == FALSE {
        let error = std::io::Error::last_os_error();
        unsafe { winapi::um::handleapi::CloseHandle(port) };
        return Err(error);
    }

    let port_address = port as usize;
    std::thread::Builder::new()
        .name("rp-job-iocp-pump".to_owned())
        .spawn(move || iocp_pump_loop(port_address, emit, direct_pid))
        .map_err(|error| {
            unsafe { winapi::um::handleapi::CloseHandle(port) };
            std::io::Error::other(format!("spawn IOCP pump thread: {error}"))
        })?;
    Ok(port_address)
}

fn iocp_pump_loop(
    port_address: usize,
    emit: Box<dyn Fn(DescendantEvent) + Send>,
    direct_pid: u32,
) {
    use winapi::shared::minwindef::{DWORD, FALSE, LPDWORD};
    use winapi::um::ioapiset::GetQueuedCompletionStatus;
    use winapi::um::minwinbase::LPOVERLAPPED;

    const ACTIVE_PROCESS_ZERO: u32 = 4;
    const NEW_PROCESS: u32 = 6;
    const EXIT_PROCESS: u32 = 7;
    const ABNORMAL_EXIT_PROCESS: u32 = 8;
    let port = port_address as winapi::shared::ntdef::HANDLE;
    loop {
        let mut message: DWORD = 0;
        let mut completion_key: usize = 0;
        let mut overlapped: LPOVERLAPPED = std::ptr::null_mut();
        let ok = unsafe {
            GetQueuedCompletionStatus(
                port,
                &raw mut message as LPDWORD,
                &raw mut completion_key as *mut _,
                &raw mut overlapped,
                winapi::um::winbase::INFINITE,
            )
        };
        if ok == FALSE {
            emit(DescendantEvent::Completed);
            break;
        }
        let pid = overlapped as usize as u32;
        match message {
            // The job-object IOCP notification is PID-only; resolving the
            // parent would need a toolhelp scan per event, racy for
            // short-lived processes. Report unknown instead.
            NEW_PROCESS if pid != direct_pid => emit(DescendantEvent::Started {
                pid,
                parent_pid: None,
            }),
            EXIT_PROCESS | ABNORMAL_EXIT_PROCESS if pid != direct_pid => {
                emit(DescendantEvent::Exited(pid));
            }
            ACTIVE_PROCESS_ZERO => {
                emit(DescendantEvent::Completed);
                break;
            }
            _ => {}
        }
    }
}

/// How often the snapshot monitor re-reads the process table.
const SNAPSHOT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Creation time of `pid` as a `FILETIME` value, or `None` if it cannot be read
/// (gone, or a process this caller may not query).
fn process_creation_time(pid: u32) -> Option<u64> {
    use winapi::shared::minwindef::FILETIME;
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::processthreadsapi::{GetProcessTimes, OpenProcess};
    use winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION;

    // SAFETY: opens a query-only handle; it is closed on every path below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    // SAFETY: FILETIME is plain data, and `handle` is live.
    let (created, ok) = unsafe {
        let mut creation: FILETIME = std::mem::zeroed();
        let mut exit: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        let ok = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
        (
            (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime),
            ok != 0,
        )
    };
    // SAFETY: closes the handle opened above.
    unsafe { CloseHandle(handle) };
    ok.then_some(created)
}

/// `(pid, parent_pid)` for every process on the machine, or `None` if the
/// snapshot could not be taken.
fn process_table() -> Option<Vec<(u32, u32)>> {
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::tlhelp32::{
        CreateToolhelp32Snapshot, Process32First, Process32Next, PROCESSENTRY32, TH32CS_SNAPPROCESS,
    };

    // SAFETY: a plain snapshot call; the handle is closed before returning.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    // SAFETY: PROCESSENTRY32 is plain data; `dwSize` is set before first use.
    let mut entry: PROCESSENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;
    let mut table = Vec::new();
    // SAFETY: `snapshot` is live and `entry` is initialised as required.
    let mut more = unsafe { Process32First(snapshot, &mut entry) } != 0;
    while more {
        table.push((entry.th32ProcessID, entry.th32ParentProcessID));
        // SAFETY: as above.
        more = unsafe { Process32Next(snapshot, &mut entry) } != 0;
    }
    // SAFETY: closes the snapshot created above.
    unsafe { CloseHandle(snapshot) };
    Some(table)
}

/// Watch the descendants of an *already-running* `root_pid` by polling the
/// process table (#1015).
///
/// This is the post-hoc counterpart of the Job Object completion port wired at
/// spawn, which a caller who did not spawn the root does not have. It needs no
/// privilege and no dependency, and its grade is inferred from snapshots: a
/// process that starts and exits between two polls is not seen.
pub fn start_snapshot_descendant_monitor(
    root_pid: u32,
    stop: std::sync::Arc<crate::platform::process::DescendantMonitorStop>,
    emit: Box<dyn Fn(DescendantEvent) + Send>,
) -> std::io::Result<()> {
    let Some(root_created) = process_creation_time(root_pid) else {
        emit(DescendantEvent::Completed);
        return Ok(());
    };
    std::thread::Builder::new()
        .name("rp-win-descsnap".to_string())
        .spawn(move || {
            let mut last = std::collections::HashMap::new();
            crate::descendant_snapshot::pump(
                || stop.is_stopped(),
                || {
                    // The root is gone, or its pid now names a different process.
                    if process_creation_time(root_pid) != Some(root_created) {
                        return None;
                    }
                    // A failed snapshot keeps the previous view: a transient
                    // error must not read as "everything exited".
                    if let Some(table) = process_table() {
                        last = crate::descendant_snapshot::descendants(
                            root_pid,
                            root_created,
                            &table,
                            &mut process_creation_time,
                        );
                    }
                    Some(last.clone())
                },
                emit.as_ref(),
                || stop.wait_timeout(SNAPSHOT_POLL_INTERVAL),
            );
        })
        .map(|_| ())
        .map_err(|error| std::io::Error::other(format!("spawn snapshot monitor: {error}")))
}
