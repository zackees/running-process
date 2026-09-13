//! Linux scheduler + private helper handshake. The rollback guard remains
//! armed until the actual target has been identified, placed and committed.
use super::{
    process_inspect::ProcessLiveness, resource_placement::Placement,
    scheduler_launch::ScheduledUnit,
};
use crate::platform::independent_spawn::{
    check, is_ready, receive, send, Channel, LaunchSpec, Message, LEASE,
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
    control: Control,
}

enum Control {
    Scheduler(ScheduledUnit),
    Broker {
        channel: Channel,
        _broker: ProcessLiveness,
    },
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
        match &mut self.control {
            Control::Scheduler(unit) => {
                if let Err(error) = self.process.signal_pinned(libc::SIGKILL) {
                    if error.raw_os_error() != Some(libc::ESRCH) {
                        return Err(error);
                    }
                }
                unit.stop(deadline)
            }
            Control::Broker { channel, .. } => {
                use super::independent_broker_wire::{receive, send, wire, Body};
                if !self.process.is_alive() {
                    return Ok(());
                }
                let cancelled = AtomicBool::new(false);
                let result = (|| {
                    send(channel, Body::Stop(wire::Empty {}), deadline, &cancelled)?;
                    if !matches!(receive(channel, deadline, &cancelled)?, Body::Stopped(_)) {
                        return Err(io::Error::from(io::ErrorKind::InvalidData));
                    }
                    Ok(())
                })();
                if result.is_err() {
                    let _ = self.process.signal_pinned(libc::SIGKILL);
                }
                result
            }
        }
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
    while !is_ready(&spec.readiness)? {
        check(deadline, cancelled)?;
        if !process.is_alive() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "target exited before readiness",
            ));
        }
        std::thread::sleep(
            Duration::from_millis(2).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
    // Revalidate identity and placement after application startup code ran.
    if !Placement::capture_pinned(&process)?.outside_worker(&worker)? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "target changed containment during startup",
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
    Ok(IndependentChild {
        process,
        control: Control::Scheduler(unit),
    })
}

/// Connect only to an already-running broker verified outside the worker.
pub fn spawn_broker(
    spec: &LaunchSpec,
    address: &str,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> io::Result<IndependentChild> {
    use super::independent_broker_wire::{encode_spec, failure_into_io, receive, send, wire, Body};
    use crate::platform::ipc::Stream;
    if timeout.is_zero() || timeout > LEASE || !Path::new(address).is_absolute() {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let deadline = Instant::now() + timeout;
    check(deadline, cancelled)?;
    spec.validate()?;
    let worker = Placement::capture(std::process::id())?;
    let endpoint = Endpoint::new(address)?;
    let stream = Stream::connect_bounded(&endpoint, deadline, cancelled).map_err(|error| {
        if matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        ) {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "pre-existing broker unavailable",
            )
        } else {
            error
        }
    })?;
    let peer = stream.peer_identity()?;
    if peer.pid == 0 || peer.user_id != current_user_id()? {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    let broker = ProcessLiveness::open_pinned(peer.pid)?;
    if !Placement::capture_pinned(&broker)?.outside_worker(&worker)? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "broker retained caller containment",
        ));
    }
    let mut channel = Channel::new(stream)?;
    let mut launch = encode_spec(spec);
    launch.timeout_millis = deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .clamp(1, 30000) as u32;
    send(&mut channel, Body::Launch(launch), deadline, cancelled)?;
    let pid = match receive(&mut channel, deadline, cancelled)? {
        Body::Started(pid) => pid,
        Body::Failed(kind) => return Err(failure_into_io(kind)),
        _ => return Err(io::Error::from(io::ErrorKind::InvalidData)),
    };
    struct Pending(Option<ProcessLiveness>);
    impl Drop for Pending {
        fn drop(&mut self) {
            if let Some(process) = &self.0 {
                let _ = process.signal_pinned(libc::SIGKILL);
            }
        }
    }
    let process = ProcessLiveness::open_pinned(pid)?;
    // Do not arm rollback termination for an arbitrary PID named by a broken
    // or mismatched broker. This protocol launches a direct broker-owned child.
    process.signal_pinned(0)?;
    broker.signal_pinned(0)?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let parent = stat
        .rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u32>().ok());
    if parent != Some(broker.pid()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "broker does not own reported target",
        ));
    }
    process.signal_pinned(0)?;
    broker.signal_pinned(0)?;
    let mut pending = Pending(Some(process));
    let process = pending
        .0
        .as_ref()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    if !Placement::capture_pinned(process)?.outside_worker(&worker)? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "broker target retained caller containment",
        ));
    }
    if !is_ready(&spec.readiness)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker target is not ready",
        ));
    }
    send(
        &mut channel,
        Body::Commit(wire::Empty {}),
        deadline,
        cancelled,
    )?;
    if !matches!(
        receive(&mut channel, deadline, cancelled)?,
        Body::Committed(_)
    ) {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    check(deadline, cancelled)?;
    let process = pending
        .0
        .take()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    Ok(IndependentChild {
        process,
        control: Control::Broker {
            channel,
            _broker: broker,
        },
    })
}
