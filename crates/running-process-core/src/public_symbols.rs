#![allow(improper_ctypes_definitions)]

use super::*;

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_process_start_public(
    process: &NativeProcess,
) -> Result<(), ProcessError> {
    process.start_impl()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_process_wait_public(
    process: &NativeProcess,
    timeout: Option<Duration>,
) -> Result<i32, ProcessError> {
    process.wait_impl(timeout)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_process_kill_public(
    process: &NativeProcess,
) -> Result<(), ProcessError> {
    process.kill_impl()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_process_close_public(
    process: &NativeProcess,
) -> Result<(), ProcessError> {
    process.close_impl()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_process_read_combined_public(
    process: &NativeProcess,
    timeout: Option<Duration>,
) -> ReadStatus<StreamEvent> {
    process.read_combined_impl(timeout)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_process_wait_for_capture_completion_public(process: &NativeProcess) {
    process.wait_for_capture_completion_impl();
}

#[cfg(windows)]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_assign_child_to_windows_kill_on_close_job_public(
    child: &Child,
) -> Result<WindowsJobHandle, std::io::Error> {
    assign_child_to_windows_kill_on_close_job_impl(child)
}
