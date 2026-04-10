use super::*;

#[inline(never)]
pub(super) fn input_payload(data: &[u8]) -> Vec<u8> {
    running_process_core::rp_rust_debug_scope!("running_process_py::pty_windows::input_payload");
    windows_terminal_input_payload(data)
}

#[inline(never)]
pub(super) fn respond_to_queries(process: &NativePtyProcess, data: &[u8]) -> PyResult<()> {
    running_process_core::rp_rust_debug_scope!(
        "running_process_py::pty_windows::respond_to_queries"
    );
    let mut guard = process.handles.lock().expect("pty handles mutex poisoned");
    let handles = guard
        .as_mut()
        .ok_or_else(|| PyRuntimeError::new_err("Pseudo-terminal process is not running"))?;
    let query = b"\x1b[6n";
    let count = data
        .windows(query.len())
        .filter(|window| *window == query)
        .count();
    for _ in 0..count {
        handles.writer.write_all(b"\x1b[1;1R").map_err(to_py_err)?;
    }
    handles.writer.flush().map_err(to_py_err)
}

#[inline(never)]
pub(super) fn send_interrupt(process: &NativePtyProcess) -> PyResult<()> {
    running_process_core::rp_rust_debug_scope!("running_process_py::pty_windows::send_interrupt");
    process.write(&[0x03])
}

#[inline(never)]
pub(super) fn terminate(process: &NativePtyProcess) -> PyResult<()> {
    running_process_core::rp_rust_debug_scope!("running_process_py::pty_windows::terminate");
    kill(process)
}

#[inline(never)]
pub(super) fn kill(process: &NativePtyProcess) -> PyResult<()> {
    running_process_core::rp_rust_debug_scope!("running_process_py::pty_windows::kill");
    let mut guard = process.handles.lock().expect("pty handles mutex poisoned");
    let handles = guard
        .take()
        .ok_or_else(|| PyRuntimeError::new_err("Pseudo-terminal process is not running"))?;
    drop(guard);

    let NativePtyHandles {
        master,
        writer,
        mut child,
        _job,
    } = handles;

    if let Err(err) = child.kill() {
        if !is_ignorable_process_control_error(&err) {
            return Err(to_py_err(err));
        }
    }
    drop(writer);
    drop(master);
    let status = child.wait().map_err(to_py_err)?;
    drop(child);
    drop(_job);
    process.store_returncode(portable_exit_code(status));
    Ok(())
}

#[inline(never)]
pub(super) fn terminate_tree(process: &NativePtyProcess) -> PyResult<()> {
    running_process_core::rp_rust_debug_scope!("running_process_py::pty_windows::terminate_tree");
    terminate(process)
}

#[inline(never)]
pub(super) fn kill_tree(process: &NativePtyProcess) -> PyResult<()> {
    running_process_core::rp_rust_debug_scope!("running_process_py::pty_windows::kill_tree");
    kill(process)
}
