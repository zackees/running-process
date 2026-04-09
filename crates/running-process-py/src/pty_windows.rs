use super::*;

pub(super) fn input_payload(data: &[u8]) -> Vec<u8> {
    windows_terminal_input_payload(data)
}

pub(super) fn respond_to_queries(process: &NativePtyProcess, data: &[u8]) -> PyResult<()> {
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

pub(super) fn send_interrupt(process: &NativePtyProcess) -> PyResult<()> {
    process.write(&[0x03])
}

pub(super) fn terminate(process: &NativePtyProcess) -> PyResult<()> {
    kill(process)
}

pub(super) fn kill(process: &NativePtyProcess) -> PyResult<()> {
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

    let _ = child.kill();
    drop(writer);
    drop(master);
    let status = child.wait().map_err(to_py_err)?;
    drop(child);
    drop(_job);
    process.store_returncode(portable_exit_code(status));
    Ok(())
}

pub(super) fn terminate_tree(process: &NativePtyProcess) -> PyResult<()> {
    terminate(process)
}

pub(super) fn kill_tree(process: &NativePtyProcess) -> PyResult<()> {
    kill(process)
}
