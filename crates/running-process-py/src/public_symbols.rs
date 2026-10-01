#![allow(improper_ctypes_definitions)]

use pyo3::prelude::*;

use crate::helpers::to_py_err;
use crate::priority::native_apply_process_nice_impl;
use crate::process::NativeRunningProcess;

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_apply_process_nice_public(pid: u32, nice: i32) -> PyResult<()> {
    native_apply_process_nice_impl(pid, nice)
}

// The two `rp_windows_*` exports are pinned by the tiny-PDB symbol list, so
// they keep their names. They are host-neutral now: the host selection lives in
// `platform::process`, and these are the stable frames on the call path.
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_windows_apply_process_priority_public(pid: u32, nice: i32) -> PyResult<()> {
    running_process_platform_internal::platform::process::apply_process_priority(pid, nice)
        .map_err(to_py_err)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_windows_generate_console_ctrl_break_public(
    pid: u32,
    creationflags: Option<u32>,
    create_process_group: bool,
) -> PyResult<()> {
    running_process_platform_internal::platform::process::send_interrupt(
        pid,
        creationflags,
        create_process_group,
    )
    .map_err(to_py_err)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_running_process_start_public(
    process: &NativeRunningProcess,
) -> PyResult<()> {
    process.start_impl()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_running_process_wait_public(
    process: &NativeRunningProcess,
    py: Python<'_>,
    timeout: Option<f64>,
) -> PyResult<i32> {
    process.wait_impl(py, timeout)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_running_process_kill_public(
    process: &NativeRunningProcess,
) -> PyResult<()> {
    process.kill_impl()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_running_process_terminate_public(
    process: &NativeRunningProcess,
) -> PyResult<()> {
    process.terminate_impl()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_running_process_close_public(
    process: &NativeRunningProcess,
    py: Python<'_>,
) -> PyResult<()> {
    process.close_impl(py)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_running_process_send_interrupt_public(
    process: &NativeRunningProcess,
) -> PyResult<()> {
    process.send_interrupt_impl()
}
