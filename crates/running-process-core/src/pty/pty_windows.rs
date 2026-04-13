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
    let handles = guard.as_mut().ok_or(PtyError::NotRunning)?;
    let query = b"\x1b[6n";
    let count = data
        .windows(query.len())
        .filter(|window| *window == query)
        .count();
    for _ in 0..count {
        handles
            .writer
            .write_all(b"\x1b[1;1R")
            .map_err(PtyError::Io)?;
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
    // On Windows/ConPTY, `close_impl()` is the stable teardown path because it
    // closes the PTY endpoints before reaping the child. Reuse that behavior
    // here instead of duplicating a more fragile kill sequence.
    process.close_impl()
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
