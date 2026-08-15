//! Launch-time Linux `ptrace` process-tree supervision.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::process::Child;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::platform::process::{ExactTraceEvent, ExactTraceEventKind, TraceOriginArtifact};

const STACK_CAPTURE_BYTES: usize = 16 * 1024;

/// Arrange for a successful `exec` to stop the child before user code runs.
/// No pre-exec SIGSTOP is used: that would deadlock `Command::spawn`'s exec
/// error pipe.
pub fn configure_exact_trace(command: &mut std::process::Command) -> io::Result<()> {
    unsafe {
        command.pre_exec(|| {
            let result = libc::ptrace(
                libc::PTRACE_TRACEME,
                0,
                std::ptr::null_mut::<libc::c_void>(),
                std::ptr::null_mut::<libc::c_void>(),
            );
            if result == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

#[derive(Default)]
struct RootState {
    exit_code: Option<i32>,
    done: bool,
    tracer_error: Option<String>,
}

struct Shared {
    state: Mutex<RootState>,
    wake: Condvar,
}

/// Root-process control handle whose wait state is owned by the tracer.
pub struct TracedChild {
    pid: u32,
    shared: Arc<Shared>,
}

impl TracedChild {
    pub fn id(&self) -> u32 {
        self.pid
    }

    pub fn try_wait_code(&self) -> io::Result<Option<i32>> {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(error) = state.tracer_error.as_ref() {
            return Err(io::Error::other(error.clone()));
        }
        Ok(state.exit_code)
    }

    pub fn kill(&self) -> io::Result<()> {
        if unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGKILL) } == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }
}

pub fn start_exact_trace(
    child: Child,
    emit: Box<dyn Fn(ExactTraceEvent) + Send>,
) -> io::Result<TracedChild> {
    let pid = child.id();
    let shared = Arc::new(Shared {
        state: Mutex::new(RootState::default()),
        wake: Condvar::new(),
    });
    let thread_shared = Arc::clone(&shared);
    let (setup_tx, setup_rx) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("rp-linux-ptrace".to_owned())
        .spawn(move || trace_loop(child, thread_shared, emit, setup_tx));
    if let Err(error) = spawned {
        // The child has already reached its TRACEME exec stop. If the one
        // thread that can own its wait/continue loop cannot be created, do
        // not strand a permanently stopped process.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
            libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::__WALL);
        }
        return Err(io::Error::other(format!(
            "spawn ptrace supervisor: {error}"
        )));
    }
    setup_rx
        .recv()
        .map_err(|_| io::Error::other("ptrace supervisor ended during launch setup"))??;
    Ok(TracedChild { pid, shared })
}

#[derive(Clone)]
struct Tracee {
    parent_pid: Option<u32>,
    parent_start_key: Option<u64>,
    start_key: Option<u64>,
    process_leader: bool,
    executable: Option<std::path::PathBuf>,
    argv: Option<Vec<OsString>>,
    origin: Option<TraceOriginArtifact>,
}

fn trace_loop(
    mut child: Child,
    shared: Arc<Shared>,
    emit: Box<dyn Fn(ExactTraceEvent) + Send>,
    setup: std::sync::mpsc::SyncSender<io::Result<()>>,
) {
    let root_pid = child.id();
    let mut initial_status = 0;
    if unsafe { libc::waitpid(root_pid as libc::pid_t, &mut initial_status, libc::__WALL) } == -1
        || !libc::WIFSTOPPED(initial_status)
    {
        let message = "root did not reach its initial ptrace exec stop";
        let _ = setup.send(Err(io::Error::other(message)));
        cleanup_failed_setup(&mut child);
        return;
    }

    let options = libc::PTRACE_O_TRACEFORK
        | libc::PTRACE_O_TRACEVFORK
        | libc::PTRACE_O_TRACECLONE
        | libc::PTRACE_O_TRACEEXEC
        | libc::PTRACE_O_TRACEEXIT;
    if ptrace_value(libc::PTRACE_SETOPTIONS, root_pid, 0, options as usize).is_err() {
        let message = "PTRACE_SETOPTIONS was denied";
        let _ = setup.send(Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            message,
        )));
        cleanup_failed_setup(&mut child);
        return;
    }

    let mut sequence = 1u64;
    let mut tracees = HashMap::from([(
        root_pid,
        Tracee {
            parent_pid: None,
            parent_start_key: None,
            start_key: process_start_key(root_pid),
            process_leader: true,
            executable: read_executable(root_pid),
            argv: read_argv(root_pid),
            origin: None,
        },
    )]);
    let root_exec = event_for(
        sequence,
        root_pid,
        &tracees[&root_pid],
        ExactTraceEventKind::Exec,
    );
    if ptrace_value(libc::PTRACE_CONT, root_pid, 0, 0).is_err() {
        let message = "failed to continue root after initial exec stop";
        let _ = setup.send(Err(io::Error::other(message)));
        detach_all(tracees.keys().copied());
        cleanup_failed_setup(&mut child);
        return;
    }
    let _ = setup.send(Ok(()));
    emit(root_exec);
    sequence += 1;

    let mut fresh_children = HashSet::new();
    while !tracees.is_empty() {
        let pids: Vec<u32> = tracees.keys().copied().collect();
        let mut progressed = false;
        for pid in pids {
            let mut status = 0;
            let waited = unsafe {
                libc::waitpid(
                    pid as libc::pid_t,
                    &mut status,
                    libc::WNOHANG | libc::__WALL,
                )
            };
            if waited == 0 {
                continue;
            }
            if waited == -1 {
                if io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
                    tracees.remove(&pid);
                    continue;
                }
                let error = io::Error::last_os_error();
                emit(ExactTraceEvent {
                    sequence,
                    pid,
                    parent_pid: tracees.get(&pid).and_then(|tracee| tracee.parent_pid),
                    parent_start_key: tracees
                        .get(&pid)
                        .and_then(|tracee| tracee.parent_start_key),
                    start_key: tracees.get(&pid).and_then(|tracee| tracee.start_key),
                    timestamp: std::time::SystemTime::now(),
                    kind: ExactTraceEventKind::Loss {
                        reason: error.to_string(),
                    },
                    executable: None,
                    argv: None,
                    origin: None,
                });
                sequence += 1;
                let _ = ptrace_value(libc::PTRACE_DETACH, pid, 0, 0);
                tracees.remove(&pid);
                if pid == root_pid {
                    finish_untraced(&mut child, &shared, "ptrace wait failed");
                    return;
                }
                continue;
            }
            progressed = true;
            if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
                if let Some(tracee) = tracees.remove(&pid) {
                    if tracee.process_leader {
                        let exit_code = libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status));
                        let signal = libc::WIFSIGNALED(status).then(|| libc::WTERMSIG(status));
                        emit(event_for(
                            sequence,
                            pid,
                            &tracee,
                            ExactTraceEventKind::Exit {
                                exit_code,
                                signal,
                                raw_status: i64::from(status),
                            },
                        ));
                        sequence += 1;
                        if pid == root_pid {
                            let normalized = exit_code.unwrap_or_else(|| 128 + signal.unwrap_or(0));
                            finish_root(&shared, normalized);
                        }
                    }
                }
                continue;
            }
            if !libc::WIFSTOPPED(status) {
                continue;
            }

            let signal = libc::WSTOPSIG(status);
            let ptrace_event = status >> 16;
            match ptrace_event {
                libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK | libc::PTRACE_EVENT_CLONE => {
                    let mut child_pid = 0usize;
                    if ptrace_value(
                        libc::PTRACE_GETEVENTMSG,
                        pid,
                        0,
                        (&raw mut child_pid) as usize,
                    )
                    .is_ok()
                    {
                        let child_pid = child_pid as u32;
                        let process_leader = thread_group_id(child_pid) == Some(child_pid);
                        let parent_pid =
                            process_leader.then(|| thread_group_id(pid).unwrap_or(pid));
                        let tracee = Tracee {
                            parent_pid,
                            parent_start_key: parent_pid.and_then(process_start_key),
                            start_key: process_start_key(child_pid),
                            process_leader,
                            executable: None,
                            argv: None,
                            origin: process_leader.then(|| capture_origin(pid)),
                        };
                        let spawn_event = process_leader.then(|| {
                            event_for(
                                sequence,
                                child_pid,
                                &tracee,
                                ExactTraceEventKind::Spawn,
                            )
                        });
                        tracees.insert(child_pid, tracee);
                        fresh_children.insert(child_pid);
                        // Resume the spawning thread before delivery, file I/O,
                        // or symbolization can run in a consumer.
                        let _ = ptrace_value(libc::PTRACE_CONT, pid, 0, 0);
                        if let Some(spawn_event) = spawn_event {
                            emit(spawn_event);
                            sequence += 1;
                        }
                    } else {
                        let _ = ptrace_value(libc::PTRACE_CONT, pid, 0, 0);
                    }
                }
                libc::PTRACE_EVENT_EXEC => {
                    let exec_event = if let Some(tracee) = tracees.get_mut(&pid) {
                        tracee.executable = read_executable(pid);
                        tracee.argv = read_argv(pid);
                        tracee
                            .process_leader
                            .then(|| event_for(sequence, pid, tracee, ExactTraceEventKind::Exec))
                    } else {
                        None
                    };
                    let _ = ptrace_value(libc::PTRACE_CONT, pid, 0, 0);
                    if let Some(exec_event) = exec_event {
                        emit(exec_event);
                        sequence += 1;
                    }
                }
                libc::PTRACE_EVENT_EXIT => {
                    let _ = ptrace_value(libc::PTRACE_CONT, pid, 0, 0);
                }
                _ => {
                    let forwarded = if fresh_children.remove(&pid) && signal == libc::SIGSTOP {
                        0
                    } else if signal == libc::SIGTRAP {
                        0
                    } else {
                        signal
                    };
                    let _ = ptrace_value(libc::PTRACE_CONT, pid, 0, forwarded as usize);
                }
            }
        }
        if !progressed {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    drop(child);
}

fn ptrace_value(request: u32, pid: u32, address: usize, data: usize) -> io::Result<libc::c_long> {
    let value = unsafe {
        libc::ptrace(
            request,
            pid as libc::pid_t,
            address as *mut libc::c_void,
            data as *mut libc::c_void,
        )
    };
    if value == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}

fn capture_origin(thread_id: u32) -> TraceOriginArtifact {
    let mut registers = vec![0u8; 1024];
    let mut iov = libc::iovec {
        iov_base: registers.as_mut_ptr().cast(),
        iov_len: registers.len(),
    };
    if ptrace_value(
        libc::PTRACE_GETREGSET,
        thread_id,
        libc::NT_PRSTATUS as usize,
        (&raw mut iov) as usize,
    )
    .is_ok()
    {
        registers.truncate(iov.iov_len);
    } else {
        registers.clear();
    }
    let (stack_pointer, instruction_pointer) = read_syscall_pointers(thread_id);
    let mut stack = vec![0u8; STACK_CAPTURE_BYTES];
    if let Some(stack_pointer) = stack_pointer {
        let local = libc::iovec {
            iov_base: stack.as_mut_ptr().cast(),
            iov_len: stack.len(),
        };
        let remote = libc::iovec {
            iov_base: stack_pointer as *mut libc::c_void,
            iov_len: stack.len(),
        };
        let read = unsafe {
            libc::process_vm_readv(thread_id as libc::pid_t, &local, 1, &remote, 1, 0)
        };
        if read >= 0 {
            stack.truncate(read as usize);
        } else {
            stack.clear();
        }
    } else {
        stack.clear();
    }
    TraceOriginArtifact {
        thread_id,
        registers,
        stack_pointer,
        instruction_pointer,
        truncated: stack.len() == STACK_CAPTURE_BYTES,
        stack,
    }
}

fn read_syscall_pointers(pid: u32) -> (Option<u64>, Option<u64>) {
    let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/syscall")) else {
        return (None, None);
    };
    let fields: Vec<&str> = text.split_ascii_whitespace().collect();
    if fields.len() < 2 {
        return (None, None);
    }
    let parse = |value: &str| u64::from_str_radix(value.trim_start_matches("0x"), 16).ok();
    (parse(fields[fields.len() - 2]), parse(fields[fields.len() - 1]))
}

fn read_executable(pid: u32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

fn read_argv(pid: u32) -> Option<Vec<OsString>> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        bytes
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| OsString::from_vec(part.to_vec()))
            .collect(),
    )
}

fn process_start_key(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.get(stat.rfind(')')? + 1..)?
        .split_ascii_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

fn thread_group_id(pid: u32) -> Option<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("Tgid:")?.trim().parse().ok())
}

fn event_for(
    sequence: u64,
    pid: u32,
    tracee: &Tracee,
    kind: ExactTraceEventKind,
) -> ExactTraceEvent {
    ExactTraceEvent {
        sequence,
        pid,
        parent_pid: tracee.parent_pid,
        parent_start_key: tracee.parent_start_key,
        start_key: tracee.start_key,
        timestamp: std::time::SystemTime::now(),
        kind,
        executable: tracee.executable.clone(),
        argv: tracee.argv.clone(),
        origin: tracee.origin.clone(),
    }
}

fn detach_all(tracees: impl IntoIterator<Item = u32>) {
    for pid in tracees {
        let _ = ptrace_value(libc::PTRACE_DETACH, pid, 0, 0);
    }
}

fn finish_untraced(child: &mut Child, shared: &Shared, message: &str) {
    let result = child.wait();
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    match result {
        Ok(status) => state.exit_code = Some(super::exit_code(status)),
        Err(error) => state.tracer_error = Some(format!("{message}: {error}")),
    }
    state.done = true;
    shared.wake.notify_all();
}

fn cleanup_failed_setup(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn finish_root(shared: &Shared, exit_code: i32) {
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    state.exit_code = Some(exit_code);
    state.done = true;
    shared.wake.notify_all();
}
