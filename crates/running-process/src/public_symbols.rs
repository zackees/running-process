#![allow(improper_ctypes_definitions)]

use std::time::Instant;

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

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_process_wait_for_capture_completion_with_deadline_public(
    process: &NativeProcess,
    deadline: Instant,
) -> bool {
    process.wait_for_capture_completion_with_deadline_impl(deadline)
}

#[cfg(windows)]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_assign_child_to_windows_kill_on_close_job_public(
    child: &Child,
) -> Result<WindowsJobHandle, std::io::Error> {
    assign_child_to_windows_kill_on_close_job_impl(child)
}

#[cfg(windows)]
#[inline(never)]
/// #539 slice 2 — observer-aware Job Object setup. When
/// `descendant_sink` is `Some`, the returned handle also owns an IOCP and
/// pump thread that emits descendant-lifecycle events. This is `extern
/// "Rust"` (not "C") because the `Sender<ObserverEvent>` parameter is not
/// ABI-stable; the older `_public` symbol is the C-ABI export.
pub fn rp_assign_child_to_windows_kill_on_close_job_with_observer_public(
    child: &Child,
    descendant_sink: Option<std::sync::mpsc::Sender<crate::observer::ObserverEvent>>,
    direct_pid: u32,
) -> Result<WindowsJobHandle, std::io::Error> {
    crate::windows::assign_child_to_windows_kill_on_close_job_with_observer_impl(
        child,
        descendant_sink,
        direct_pid,
    )
}
