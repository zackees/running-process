use pyo3::prelude::*;

use crate::public_symbols;

#[pyfunction]
#[inline(never)]
pub(crate) fn native_apply_process_nice(pid: u32, nice: i32) -> PyResult<()> {
    public_symbols::rp_native_apply_process_nice_public(pid, nice)
}

pub(crate) fn native_apply_process_nice_impl(pid: u32, nice: i32) -> PyResult<()> {
    running_process::rp_rust_debug_scope!("running_process_py::native_apply_process_nice");
    // The nice-to-priority mapping (setpriority on Unix, priority classes on
    // Windows) lives behind the platform facade.
    public_symbols::rp_windows_apply_process_priority_public(pid, nice)
}
