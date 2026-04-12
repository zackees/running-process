#![allow(improper_ctypes_definitions)]

use super::*;

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_apply_process_nice_public(pid: u32, nice: i32) -> PyResult<()> {
    native_apply_process_nice_impl(pid, nice)
}

#[cfg(windows)]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_windows_apply_process_priority_public(pid: u32, nice: i32) -> PyResult<()> {
    windows_apply_process_priority_impl(pid, nice)
}

#[cfg(windows)]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_windows_generate_console_ctrl_break_public(
    pid: u32,
    creationflags: Option<u32>,
) -> PyResult<()> {
    windows_generate_console_ctrl_break_impl(pid, creationflags)
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

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_pty_process_start_public(process: &NativePtyProcess) -> PyResult<()> {
    process.start_impl()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_pty_process_respond_to_queries_public(
    process: &NativePtyProcess,
    data: &[u8],
) -> PyResult<()> {
    process.respond_to_queries_impl(data)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_pty_process_resize_public(
    process: &NativePtyProcess,
    rows: u16,
    cols: u16,
) -> PyResult<()> {
    process.resize_impl(rows, cols)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_pty_process_send_interrupt_public(
    process: &NativePtyProcess,
) -> PyResult<()> {
    process.send_interrupt_impl()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_pty_process_wait_public(
    process: &NativePtyProcess,
    timeout: Option<f64>,
) -> PyResult<i32> {
    process.wait_impl(timeout)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_pty_process_terminate_public(
    process: &NativePtyProcess,
) -> PyResult<()> {
    process.terminate_impl()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_pty_process_kill_public(process: &NativePtyProcess) -> PyResult<()> {
    process.kill_impl()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_pty_process_terminate_tree_public(
    process: &NativePtyProcess,
) -> PyResult<()> {
    process.terminate_tree_impl()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_pty_process_kill_tree_public(
    process: &NativePtyProcess,
) -> PyResult<()> {
    process.kill_tree_impl()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_pty_process_close_public(
    process: &NativePtyProcess,
    py: Python<'_>,
) -> PyResult<()> {
    py.allow_threads(|| rp_native_pty_process_close_impl_public(process))
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_pty_process_close_impl_public(
    process: &NativePtyProcess,
) -> PyResult<()> {
    process.close_impl()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_pty_process_close_nonblocking_public(process: &NativePtyProcess) {
    process.close_nonblocking();
}

#[cfg(windows)]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_pty_windows_input_payload_public(data: &[u8]) -> Vec<u8> {
    crate::pty_windows::input_payload(data)
}

#[cfg(windows)]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_pty_windows_respond_to_queries_public(
    process: &NativePtyProcess,
    data: &[u8],
) -> PyResult<()> {
    crate::pty_windows::respond_to_queries(process, data)
}

#[cfg(windows)]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_pty_windows_send_interrupt_public(process: &NativePtyProcess) -> PyResult<()> {
    crate::pty_windows::send_interrupt(process)
}

#[cfg(windows)]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_pty_windows_terminate_public(process: &NativePtyProcess) -> PyResult<()> {
    crate::pty_windows::terminate(process)
}

#[cfg(windows)]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_pty_windows_kill_public(process: &NativePtyProcess) -> PyResult<()> {
    crate::pty_windows::kill(process)
}

#[cfg(windows)]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_pty_windows_terminate_tree_public(process: &NativePtyProcess) -> PyResult<()> {
    crate::pty_windows::terminate_tree(process)
}

#[cfg(windows)]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_pty_windows_kill_tree_public(process: &NativePtyProcess) -> PyResult<()> {
    crate::pty_windows::kill_tree(process)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_spawn_pty_reader_public(
    reader: Box<dyn Read + Send>,
    shared: Arc<PtyReadShared>,
    echo: Arc<AtomicBool>,
    idle_detector: Arc<Mutex<Option<Arc<IdleDetectorCore>>>>,
    output_bytes_total: Arc<AtomicUsize>,
    control_churn_bytes_total: Arc<AtomicUsize>,
) {
    spawn_pty_reader(
        reader,
        shared,
        echo,
        idle_detector,
        output_bytes_total,
        control_churn_bytes_total,
    );
}

#[cfg(windows)]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_py_assign_child_to_windows_kill_on_close_job_public(
    handle: Option<std::os::windows::io::RawHandle>,
) -> PyResult<WindowsJobHandle> {
    assign_child_to_windows_kill_on_close_job(handle)
}

#[cfg(windows)]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_apply_windows_pty_priority_public(
    handle: Option<std::os::windows::io::RawHandle>,
    nice: Option<i32>,
) -> PyResult<()> {
    apply_windows_pty_priority(handle, nice)
}
