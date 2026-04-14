use super::*;
use crate::{unix_signal_process, unix_signal_process_group, UnixSignal};
use sysinfo::{Pid, System};

fn system_pid(pid: u32) -> Pid {
    Pid::from_u32(pid)
}

fn descendant_pids(system: &System, pid: Pid) -> Vec<Pid> {
    use std::collections::HashMap;
    let mut children_map: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (child_pid, process) in system.processes() {
        if let Some(parent) = process.parent() {
            children_map.entry(parent).or_default().push(*child_pid);
        }
    }
    let mut descendants = Vec::new();
    let mut stack = vec![pid];
    while let Some(current) = stack.pop() {
        if let Some(children) = children_map.get(&current) {
            for &child in children {
                descendants.push(child);
                stack.push(child);
            }
        }
    }
    descendants
}

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

pub(super) fn respond_to_queries(
    _process: &NativePtyProcess,
    _data: &[u8],
) -> Result<(), PtyError> {
    Ok(())
}

pub(super) fn send_interrupt(process: &NativePtyProcess) -> Result<(), PtyError> {
    let guard = process.handles.lock().expect("pty handles mutex poisoned");
    let handles = guard.as_ref().ok_or(PtyError::NotRunning)?;
    if let Some(pid) = handles.master.process_group_leader() {
        unix_signal_process_group(pid, UnixSignal::Interrupt)?;
        return Ok(());
    }
    drop(guard);
    process.write_impl(&[0x03], false)
}

pub(super) fn terminate(process: &NativePtyProcess) -> Result<(), PtyError> {
    let mut guard = process.handles.lock().expect("pty handles mutex poisoned");
    let handles = guard.as_mut().ok_or(PtyError::NotRunning)?;
    let pid = handles.child.process_id().ok_or(PtyError::NotRunning)?;
    unix_signal_process(pid, UnixSignal::Terminate)?;
    Ok(())
}

pub(super) fn kill(process: &NativePtyProcess) -> Result<(), PtyError> {
    let mut guard = process.handles.lock().expect("pty handles mutex poisoned");
    let handles = guard.take().ok_or(PtyError::NotRunning)?;
    drop(guard);

    let NativePtyHandles {
        master,
        writer,
        mut child,
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
    process.store_returncode(portable_exit_code(status));
    process.join_reader_worker();
    process.mark_reader_closed();
    Ok(())
}

pub(super) fn terminate_tree(process: &NativePtyProcess) -> Result<(), PtyError> {
    let pid = process.pid()?.ok_or(PtyError::NotRunning)?;
    signal_tree(pid, UnixSignal::Terminate)?;
    Ok(())
}

pub(super) fn kill_tree(process: &NativePtyProcess) -> Result<(), PtyError> {
    let pid = process.pid()?.ok_or(PtyError::NotRunning)?;
    signal_tree(pid, UnixSignal::Kill)?;
    Ok(())
}
