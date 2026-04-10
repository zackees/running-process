use super::*;

fn signal_tree(pid: u32, signal: UnixSignal) -> Result<(), std::io::Error> {
    let system = System::new_all();
    let pid = system_pid(pid);
    let Some(_) = system.process(pid) else {
        return Ok(());
    };

    let mut targets = descendant_pids(&system, pid);
    targets.reverse();
    targets.push(pid);

    for target in targets {
        let raw_pid = target.as_u32();
        if let Err(err) = unix_signal_process(raw_pid, signal) {
            if !is_ignorable_process_control_error(&err) {
                return Err(err);
            }
        }
    }
    Ok(())
}

pub(super) fn input_payload(data: &[u8]) -> Vec<u8> {
    data.to_vec()
}

pub(super) fn respond_to_queries(_process: &NativePtyProcess, _data: &[u8]) -> PyResult<()> {
    Ok(())
}

pub(super) fn send_interrupt(process: &NativePtyProcess) -> PyResult<()> {
    let guard = process.handles.lock().expect("pty handles mutex poisoned");
    let handles = guard
        .as_ref()
        .ok_or_else(|| PyRuntimeError::new_err("Pseudo-terminal process is not running"))?;
    if let Some(pid) = handles.master.process_group_leader() {
        unix_signal_process_group(pid, UnixSignal::Interrupt).map_err(to_py_err)?;
        return Ok(());
    }
    drop(guard);
    process.write(&[0x03])
}

pub(super) fn terminate(process: &NativePtyProcess) -> PyResult<()> {
    let mut guard = process.handles.lock().expect("pty handles mutex poisoned");
    let handles = guard
        .as_mut()
        .ok_or_else(|| PyRuntimeError::new_err("Pseudo-terminal process is not running"))?;
    let pid = handles
        .child
        .process_id()
        .ok_or_else(|| PyRuntimeError::new_err("Pseudo-terminal process is not running"))?;
    unix_signal_process(pid, UnixSignal::Terminate).map_err(to_py_err)
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
    process.store_returncode(portable_exit_code(status));
    Ok(())
}

pub(super) fn terminate_tree(process: &NativePtyProcess) -> PyResult<()> {
    let pid = process
        .pid()?
        .ok_or_else(|| PyRuntimeError::new_err("Pseudo-terminal process is not running"))?;
    signal_tree(pid, UnixSignal::Terminate).map_err(to_py_err)
}

pub(super) fn kill_tree(process: &NativePtyProcess) -> PyResult<()> {
    let pid = process
        .pid()?
        .ok_or_else(|| PyRuntimeError::new_err("Pseudo-terminal process is not running"))?;
    signal_tree(pid, UnixSignal::Kill).map_err(to_py_err)
}
