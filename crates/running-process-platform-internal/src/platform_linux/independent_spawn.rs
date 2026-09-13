//! Linux scheduler + private helper handshake. The rollback guard remains
//! armed until the actual target has been identified, placed and committed.
use super::{
    process_inspect::ProcessLiveness, resource_placement::Placement,
    scheduler_launch::ScheduledUnit,
};
use crate::platform::independent_spawn::{
    check, receive, send, Channel, LaunchSpec, Message, LEASE,
};
use crate::platform::ipc::{current_user_id, Endpoint, Listener, ListenerNonblockingMode};
use std::{
    io,
    path::Path,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

/// A scheduler-owned daemon. Drop preserves committed detached lifetime.
/// Exit observation does not provide a parent-child numeric exit status.
pub struct IndependentChild {
    process: ProcessLiveness,
    unit: ScheduledUnit,
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
        if let Err(error) = self.process.signal_pinned(libc::SIGKILL) {
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        self.unit.stop(deadline)
    }
    pub fn wait(&self, timeout: Duration, cancelled: &AtomicBool) -> io::Result<()> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        while self.process.is_alive() {
            check(deadline, cancelled)?;
            std::thread::sleep(
                Duration::from_millis(2).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
        Ok(())
    }
}

pub fn spawn(
    spec: &LaunchSpec,
    helper: &Path,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> io::Result<IndependentChild> {
    if timeout.is_zero() || timeout > LEASE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "launch timeout must be within the 30-second helper lease",
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
    let directory = tempfile::Builder::new().prefix("rpil-").tempdir()?;
    crate::platform::private_dir::ensure_owner_private_directory(directory.path())?;
    let address = directory.path().join("s");
    let endpoint = Endpoint::new(
        address
            .to_str()
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?,
    )?;
    let listener = Listener::bind_owner_only(&endpoint)?;
    listener.set_nonblocking(ListenerNonblockingMode::Both)?;
    let worker = Placement::capture(std::process::id())?;
    let mut unit = ScheduledUnit::launch(helper, &address, deadline, cancelled)?;
    let helper_identity = unit.verify_helper_placement(&worker, deadline, cancelled)?;
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
    if peer.pid != helper_identity.pid() || peer.user_id != current_user_id()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "launcher peer identity mismatch",
        ));
    }
    helper_identity.signal_pinned(0)?;
    let mut stream = Channel::new(stream)?;
    send(
        &mut stream,
        &Message::Launch(spec.clone()),
        deadline,
        cancelled,
    )?;
    let pid = match receive(&mut stream, deadline, cancelled)? {
        Message::Started { pid } => pid,
        Message::Failed { kind } => return Err(kind.into_io()),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected target identity",
            ))
        }
    };
    let process = ProcessLiveness::open_pinned(pid)?;
    if !Placement::capture_pinned(&process)?.outside_worker(&worker)? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "target retained caller containment",
        ));
    }
    send(&mut stream, &Message::Commit, deadline, cancelled)?;
    if !matches!(
        receive(&mut stream, deadline, cancelled)?,
        Message::Committed
    ) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected target commit acknowledgement",
        ));
    }
    check(deadline, cancelled)?;
    unit.retain_on_drop();
    Ok(IndependentChild { process, unit })
}
