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
    let pid = handles.child.pid();
    if pid == 0 {
        return Err(PtyError::NotRunning);
    }
    unix_signal_process(pid, UnixSignal::Terminate)?;
    Ok(())
}

pub(super) fn kill(process: &NativePtyProcess) -> Result<(), PtyError> {
    let mut guard = process.handles.lock().expect("pty handles mutex poisoned");
    let handles = guard.take().ok_or(PtyError::NotRunning)?;
    drop(guard);
    process.finish_unix_teardown(handles)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::backend::{PtyChild, PtyMaster, PtySize};
    use crate::pty::NativePtyHandles;
    use std::fs::File;
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    struct NoGroupMaster {
        fd: OwnedFd,
    }

    impl PtyMaster for NoGroupMaster {
        fn try_clone_reader(&mut self) -> io::Result<Box<dyn Read + Send>> {
            Err(io::Error::new(io::ErrorKind::Unsupported, "unused by test"))
        }

        fn take_writer(&mut self) -> io::Result<Box<dyn Write + Send>> {
            Err(io::Error::new(io::ErrorKind::Unsupported, "unused by test"))
        }

        fn resize(&self, _size: PtySize) -> io::Result<()> {
            Ok(())
        }

        fn get_size(&self) -> io::Result<PtySize> {
            Ok(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
        }

        fn process_group_leader(&self) -> Option<i32> {
            None
        }
    }

    struct RunningChild;

    impl PtyChild for RunningChild {
        fn pid(&self) -> u32 {
            1
        }

        fn try_wait(&mut self) -> io::Result<Option<u32>> {
            Ok(None)
        }

        fn wait(&mut self) -> io::Result<u32> {
            Ok(0)
        }

        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn open_full_pty_input_queue() -> (OwnedFd, OwnedFd) {
        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0,
            "openpty failed: {}",
            io::Error::last_os_error()
        );
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };

        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_ne!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK,) },
            -1
        );
        let chunk = [b'x'; 1024];
        loop {
            let written =
                unsafe { libc::write(master.as_raw_fd(), chunk.as_ptr().cast(), chunk.len()) };
            if written >= 0 {
                continue;
            }
            let error = io::Error::last_os_error();
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            break;
        }
        assert_ne!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags) },
            -1
        );
        (master, slave)
    }

    #[test]
    fn interrupt_fallback_does_not_block_on_full_pty_input_queue() {
        const DEADLINE: Duration = Duration::from_millis(150);

        let (master, slave) = open_full_pty_input_queue();
        let writer_fd = unsafe { libc::dup(master.as_raw_fd()) };
        assert_ne!(writer_fd, -1);
        let writer = unsafe { File::from_raw_fd(writer_fd) };

        let process = NativePtyProcess::new(vec!["unused".into()], None, None, 24, 80, None)
            .expect("test process");
        *process.handles.lock().expect("handles mutex") = Some(NativePtyHandles {
            master: Box::new(NoGroupMaster { fd: master }),
            writer: Arc::new(Mutex::new(Box::new(writer))),
            child: Box::new(RunningChild),
        });

        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        let worker = std::thread::spawn(move || {
            let result = send_interrupt(&process);
            let _ = tx.send((result, started.elapsed()));
            let _ = process.handles.lock().expect("handles mutex").take();
        });

        let timely = rx.recv_timeout(DEADLINE);
        drop(slave);
        let (result, elapsed) = timely
            .or_else(|_| rx.recv_timeout(Duration::from_secs(1)))
            .expect("interrupt fallback did not unblock after closing PTY slave");
        worker.join().expect("interrupt worker panicked");

        assert!(
            elapsed < DEADLINE,
            "interrupt fallback blocked for {elapsed:?} on a full PTY input queue"
        );
        assert!(result.is_ok(), "best-effort fallback failed: {result:?}");
    }
}
