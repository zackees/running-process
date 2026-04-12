use super::*;

#[inline(never)]
pub(super) fn input_payload(data: &[u8]) -> Vec<u8> {
    crate::rp_rust_debug_scope!("running_process_core::pty_windows::input_payload");
    windows_terminal_input_payload(data)
}

#[inline(never)]
pub(super) fn respond_to_queries(process: &NativePtyProcess, data: &[u8]) -> Result<(), PtyError> {
    crate::rp_rust_debug_scope!("running_process_core::pty_windows::respond_to_queries");
    let mut guard = process.handles.lock().expect("pty handles mutex poisoned");
    let handles = guard
        .as_mut()
        .ok_or(PtyError::NotRunning)?;
    let query = b"\x1b[6n";
    let count = data
        .windows(query.len())
        .filter(|window| *window == query)
        .count();
    for _ in 0..count {
        handles.writer.write_all(b"\x1b[1;1R").map_err(PtyError::Io)?;
    }
    handles.writer.flush().map_err(PtyError::Io)
}

#[inline(never)]
pub(super) fn send_interrupt(process: &NativePtyProcess) -> Result<(), PtyError> {
    crate::rp_rust_debug_scope!("running_process_core::pty_windows::send_interrupt");
    process.write_impl(&[0x03], false)
}

#[inline(never)]
pub(super) fn terminate(process: &NativePtyProcess) -> Result<(), PtyError> {
    crate::rp_rust_debug_scope!("running_process_core::pty_windows::terminate");
    kill(process)
}

#[inline(never)]
pub(super) fn kill(process: &NativePtyProcess) -> Result<(), PtyError> {
    crate::rp_rust_debug_scope!("running_process_core::pty_windows::kill");
    let mut guard = process.handles.lock().expect("pty handles mutex poisoned");
    let handles = guard
        .take()
        .ok_or(PtyError::NotRunning)?;
    drop(guard);

    let NativePtyHandles {
        master,
        writer,
        mut child,
        _job,
    } = handles;

    if let Err(err) = child.kill() {
        if !is_ignorable_process_control_error(&err) {
            return Err(PtyError::Io(err));
        }
    }
    drop(writer);
    drop(master);
    let status = child.wait().map_err(PtyError::Io)?;
    drop(child);
    drop(_job);
    process.store_returncode(portable_exit_code(status));
    Ok(())
}

#[inline(never)]
pub(super) fn terminate_tree(process: &NativePtyProcess) -> Result<(), PtyError> {
    crate::rp_rust_debug_scope!("running_process_core::pty_windows::terminate_tree");
    terminate(process)
}

#[inline(never)]
pub(super) fn kill_tree(process: &NativePtyProcess) -> Result<(), PtyError> {
    crate::rp_rust_debug_scope!("running_process_core::pty_windows::kill_tree");
    kill(process)
}
