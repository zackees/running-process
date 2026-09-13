//! Task Scheduler-owned helper and target handshake with Job Object evidence.
use super::{
    process_inspect::{process_executable_path, process_same_executable_path, ProcessLiveness},
    scheduler_launch::ScheduledTask,
};
use crate::platform::{
    independent_spawn::{check, is_ready, receive, send, Channel, LaunchSpec, Message, LEASE},
    ipc::{current_user_id, Endpoint, Listener, ListenerNonblockingMode},
};
use std::{
    io,
    path::Path,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

/// Detached, handle-pinned target and supervisor. Dropping preserves lifetime.
pub struct IndependentChild {
    process: ProcessLiveness,
    helper: ProcessLiveness,
}
impl IndependentChild {
    pub fn id(&self) -> u32 {
        self.process.pid()
    }
    pub fn is_alive(&self) -> bool {
        self.process.is_alive()
    }
    pub fn stop(&mut self, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        self.process.terminate_pinned()?;
        self.helper.terminate_pinned()?;
        while self.process.is_alive() || self.helper.is_alive() {
            check(deadline, &AtomicBool::new(false))?;
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }
    pub fn wait(&self, timeout: Duration, cancelled: &AtomicBool) -> io::Result<()> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        while self.process.is_alive() {
            check(deadline, cancelled)?;
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }
}

struct Pending(Option<ProcessLiveness>);
impl Pending {
    fn get(&self) -> &ProcessLiveness {
        self.0.as_ref().expect("pending process owned")
    }
    fn commit(mut self) -> ProcessLiveness {
        self.0.take().expect("pending process owned")
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(process) = &self.0 {
            let _ = process.terminate_pinned();
        }
    }
}

pub fn spawn(
    spec: &LaunchSpec,
    helper_path: &Path,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> io::Result<IndependentChild> {
    if timeout.is_zero() || timeout > LEASE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "launch timeout must fit the 30-second helper lease",
        ));
    }
    let deadline = Instant::now() + timeout;
    check(deadline, cancelled)?;
    spec.validate()?;
    send(
        &mut io::sink(),
        &Message::Launch(spec.clone()),
        deadline,
        cancelled,
    )?;
    let (mut task, address) = ScheduledTask::prepare(helper_path)?;
    let endpoint = Endpoint::new(address)?;
    let listener = Listener::bind_owner_only(&endpoint)?;
    listener.set_nonblocking(ListenerNonblockingMode::Both)?;
    task.start(deadline, cancelled)?;
    let stream = loop {
        check(deadline, cancelled)?;
        match listener.accept() {
            Ok(stream) => break stream,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                std::thread::sleep(Duration::from_millis(2))
            }
            Err(error) => return Err(error),
        }
    };
    let peer = stream.peer_identity()?;
    if peer.pid == 0 || peer.user_id != current_user_id()? {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    // Pin identity first, but do not arm termination for an unauthenticated
    // peer. A same-user process connecting to the pipe is not ours to kill.
    let helper = ProcessLiveness::open_pinned(peer.pid)?;
    if !process_same_executable_path(&process_executable_path(peer.pid)?, helper_path) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unexpected launcher executable",
        ));
    }
    if !helper.is_alive() {
        return Err(io::Error::from(io::ErrorKind::BrokenPipe));
    }
    let helper = Pending(Some(helper));
    if !helper.get().outside_current_job(deadline)? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "scheduler helper retained caller Job Object",
        ));
    }
    let mut stream = Channel::new(stream)?;
    send(
        &mut stream,
        &Message::Launch(spec.clone()),
        deadline,
        cancelled,
    ).map_err(|error| io::Error::new(error.kind(), "target payload transfer failed"))?;
    let pid = match receive(&mut stream, deadline, cancelled)
        .map_err(|error| io::Error::new(error.kind(), "target identity response failed"))? {
        Message::Started { pid } => pid,
        Message::Failed { kind } => return Err(kind.into_io()),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected target identity",
            ))
        }
    };
    let process = Pending(Some(ProcessLiveness::open_pinned(pid)?));
    if !process.get().outside_current_job(deadline)? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "target retained caller Job Object",
        ));
    }
    while !is_ready(&spec.readiness)? {
        check(deadline, cancelled)?;
        if !process.get().is_alive() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "target exited before readiness",
            ));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    if !process.get().outside_current_job(deadline)? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "target changed Job Object during startup",
        ));
    }
    send(&mut stream, &Message::Commit, deadline, cancelled)?;
    if !matches!(
        receive(&mut stream, deadline, cancelled)?,
        Message::Committed
    ) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected commit acknowledgement",
        ));
    }
    // Removing a definition leaves its running instance alive. Retain pinned
    // rollback ownership until that removal and the final cancellation check.
    task.remove_definition(deadline, cancelled)?;
    check(deadline, cancelled)?;
    Ok(IndependentChild {
        process: process.commit(),
        helper: helper.commit(),
    })
}
