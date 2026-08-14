//! Windows implementation root for the process capability.

use std::ffi::OsStr;
use std::io;
use std::sync::OnceLock;

use tokio::process::{Child, Command};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER};
use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

use crate::SpawnSpec;

#[path = "platform/process_tree.rs"]
mod process_tree;

pub fn kill_tree(pid: u32, timeout: std::time::Duration) -> io::Result<u32> {
    process_tree::kill_tree(pid, timeout, process_start_key)
}

fn process_start_key(pid: sysinfo::Pid, _process: &sysinfo::Process) -> io::Result<u64> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid.as_u32()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    let queried = unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    let query_error = if queried == 0 { Some(io::Error::last_os_error()) } else { None };
    unsafe { CloseHandle(handle); }
    if let Some(error) = query_error {
        return Err(error);
    }
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

pub(crate) fn configure_command(
    command: &mut Command,
    create_process_group: bool,
    _kill_when_owner_dies: bool,
) -> io::Result<()> {
    if create_process_group {
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    Ok(())
}

pub(crate) fn after_spawn(child: &Child, kill_when_owner_dies: bool) {
    if kill_when_owner_dies {
        assign(child.raw_handle());
    }
}

pub(crate) fn signal_process(pid: u32) -> io::Result<()> {
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) { Ok(()) } else { Err(error) };
    }
    let terminated = unsafe { TerminateProcess(handle, 1) };
    let termination_error = if terminated == 0 { Some(io::Error::last_os_error()) } else { None };
    unsafe { CloseHandle(handle) };
    termination_error.map_or(Ok(()), Err)
}

pub(crate) fn signal_process_group(pid: u32) -> io::Result<()> {
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) } != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_INVALID_HANDLE as i32) { Ok(()) } else { Err(error) }
}

pub(crate) fn shell_spec(command: &OsStr) -> SpawnSpec {
    SpawnSpec::new("cmd.exe").arg("/C").arg(command)
}

struct Job(HANDLE);
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

static JOB: OnceLock<Option<Job>> = OnceLock::new();

fn create() -> Option<Job> {
    unsafe {
        let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if handle.is_null() { return None; }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(handle, JobObjectExtendedLimitInformation, &info as *const _ as *const _, std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32) == 0 { return None; }
        Some(Job(handle))
    }
}

fn assign(child: Option<HANDLE>) {
    let Some(child) = child else { return };
    let Some(job) = JOB.get_or_init(create).as_ref() else { return };
    unsafe { AssignProcessToJobObject(job.0, child); }
}
