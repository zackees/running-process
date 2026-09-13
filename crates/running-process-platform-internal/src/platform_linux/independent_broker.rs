//! Per-launch ownership inside an explicitly pre-existing Linux broker.
use super::independent_broker_wire::{decode_spec, failure, receive, send, wire, Body};
use crate::platform::independent_spawn::{spawn_inherited, Channel, LEASE};
use crate::platform::ipc::Stream;
use std::{
    io::{self, Read},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

/// Serve an explicitly provisioned owner-private endpoint. This function never
/// changes its own cgroup placement or starts another broker. At most 32 launch
/// sessions (including committed live targets) are retained at once.
pub fn run(endpoint: &str, cancelled: &AtomicBool) -> io::Result<()> {
    use crate::platform::ipc::{Endpoint, Listener, ListenerNonblockingMode};
    use std::sync::atomic::AtomicUsize;
    if cancelled.load(Ordering::Acquire) {
        return Ok(());
    }
    let endpoint = Endpoint::new(endpoint)?;
    if !std::path::Path::new(endpoint.display()).is_absolute() {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let listener = Listener::bind_owner_only(&endpoint)?;
    listener.set_nonblocking(ListenerNonblockingMode::Both)?;
    let shutdown = AtomicBool::new(false);
    let active = AtomicUsize::new(0);
    struct Active<'a>(&'a AtomicUsize);
    impl Drop for Active<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }
    std::thread::scope(|scope| {
        let result = loop {
            if cancelled.load(Ordering::Acquire) {
                break Ok(());
            }
            match listener.accept() {
                Ok(stream) => {
                    if active.fetch_add(1, Ordering::AcqRel) >= 32 {
                        active.fetch_sub(1, Ordering::AcqRel);
                        drop(stream);
                        continue;
                    }
                    let active = &active;
                    let shutdown = &shutdown;
                    if let Err(error) = std::thread::Builder::new()
                        .name("rp-independent-launch".into())
                        .spawn_scoped(scope, move || {
                            let _active = Active(active);
                            let _ = serve_connection(stream, shutdown);
                        })
                    {
                        active.fetch_sub(1, Ordering::AcqRel);
                        break Err(error);
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => break Err(error),
            }
        };
        // A fatal accept/spawn error must also release idle and committed
        // sessions before the scope joins them, rather than waiting forever.
        shutdown.store(true, Ordering::Release);
        result
    })
}

pub(super) fn serve_connection(stream: Stream, cancelled: &AtomicBool) -> io::Result<()> {
    let deadline = Instant::now() + LEASE;
    if stream.peer_identity()?.user_id != crate::platform::ipc::current_user_id()? {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    let mut stream = Channel::new(stream)?;
    let Body::Launch(spec) = receive(&mut stream, deadline, cancelled)? else {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    };
    if spec.timeout_millis == 0 || spec.timeout_millis > 30000 {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let deadline = deadline.min(Instant::now() + Duration::from_millis(spec.timeout_millis.into()));
    let mut child = match spawn_inherited(
        &decode_spec(spec),
        false,
        deadline.saturating_duration_since(Instant::now()),
        cancelled,
    ) {
        Ok(child) => child,
        Err(error) => {
            let _ = send(
                &mut stream,
                Body::Failed(failure(&error) as i32),
                Instant::now() + Duration::from_millis(100),
                &AtomicBool::new(false),
            );
            return Err(error);
        }
    };
    send(&mut stream, Body::Started(child.id()), deadline, cancelled)?;
    if !matches!(receive(&mut stream, deadline, cancelled)?, Body::Commit(_)) {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    if child.try_wait()?.is_some() {
        return Err(io::Error::from(io::ErrorKind::BrokenPipe));
    }
    send(
        &mut stream,
        Body::Committed(wire::Empty {}),
        deadline,
        cancelled,
    )?;
    // Ownership stays with this broker even after requester disconnect. Keep
    // the unreaped group leader pinned until all later stop/drop work is done.
    // Poll for the first frame byte so idle connections do not prevent exit
    // observation. Once a frame starts, retain all partial bytes under a lease.
    let mut stream = Some(stream);
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        if let Some(connection) = &mut stream {
            let mut command = [0_u8];
            match connection.read(&mut command) {
                Ok(0) => stream = None,
                Ok(_) => {
                    let control_deadline = Instant::now() + LEASE;
                    let mut input = io::Cursor::new(command).chain(&mut *connection);
                    if !matches!(
                        receive(&mut input, control_deadline, cancelled),
                        Ok(Body::Stop(_))
                    ) {
                        // Incomplete, invalid, or abandoned control input does
                        // not revoke an already committed detached lifetime.
                        stream = None;
                        continue;
                    }
                    child.stop(Duration::from_secs(2))?;
                    send(
                        connection,
                        Body::Stopped(wire::Empty {}),
                        control_deadline,
                        cancelled,
                    )?;
                    return Ok(());
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                // A vanished committed requester does not revoke detachment.
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
                    ) =>
                {
                    stream = None
                }
                Err(error) => return Err(error),
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(test)]
mod tests {
    use super::super::independent_broker_wire::encode_spec;
    use super::*;
    use crate::platform::independent_spawn::{Channel, LaunchSpec, Readiness};
    use crate::platform::ipc::{Endpoint, Listener};
    use std::io::Write;
    use std::time::{Duration, Instant};

    #[test]
    #[ignore = "target fixture invoked by the broker ownership tests"]
    fn broker_target_fixture() {
        let Some(path) = std::env::var_os("RP_BROKER_READY") else {
            return;
        };
        std::fs::write(path, b"ready").unwrap();
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn broker_listener_cancels_idle_clients_and_retires_endpoint() {
        let directory = tempfile::tempdir().unwrap();
        crate::platform::private_dir::ensure_owner_private_directory(directory.path()).unwrap();
        let path = directory.path().join("broker.sock");
        let endpoint = Endpoint::new(path.to_str().unwrap()).unwrap();
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let shutdown = std::sync::Arc::clone(&cancelled);
        let address = endpoint.display().to_owned();
        let broker = std::thread::spawn(move || run(&address, &shutdown));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !path.exists() && !broker.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        let connected = Stream::connect_bounded(&endpoint, deadline, &AtomicBool::new(false));
        cancelled.store(true, Ordering::Release);
        broker.join().unwrap().unwrap();
        assert!(
            connected.is_ok(),
            "broker did not accept an explicit connection"
        );
        assert!(!path.exists(), "broker endpoint must retire after shutdown");
    }

    #[test]
    fn broker_readiness_timeout_is_reported() {
        let directory = tempfile::tempdir().unwrap();
        crate::platform::private_dir::ensure_owner_private_directory(directory.path()).unwrap();
        let endpoint = Endpoint::new(directory.path().join("s").to_str().unwrap()).unwrap();
        let listener = Listener::bind_owner_only(&endpoint).unwrap();
        let server = std::thread::spawn(move || {
            serve_connection(listener.accept().unwrap(), &AtomicBool::new(false))
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let cancelled = AtomicBool::new(false);
        let mut channel =
            Channel::new(Stream::connect_bounded(&endpoint, deadline, &cancelled).unwrap())
                .unwrap();
        let spec = LaunchSpec {
            program: std::env::current_exe().unwrap().into_os_string(),
            args: vec![
                "--exact".into(),
                "platform_linux::independent_broker::tests::broker_target_fixture".into(),
                "--ignored".into(),
            ],
            cwd: directory.path().as_os_str().to_owned(),
            environment: vec![(
                "RP_BROKER_READY".into(),
                directory.path().join("actual-ready").into_os_string(),
            )],
            stdout: None,
            stderr: None,
            readiness: Readiness::File {
                path: directory.path().join("never-ready").into_os_string(),
                value: b"ready".to_vec(),
            },
        };
        let mut payload = encode_spec(&spec);
        payload.timeout_millis = 100;
        send(&mut channel, Body::Launch(payload), deadline, &cancelled).unwrap();
        let Body::Failed(code) = receive(&mut channel, deadline, &cancelled).unwrap() else {
            panic!("expected typed readiness failure")
        };
        assert_eq!(
            super::super::independent_broker_wire::failure_into_io(code).kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            server.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn broker_owns_target_until_commit_and_preserves_it_after_disconnect() {
        for (committed, stop, partial) in [
            (false, false, 0),
            (true, false, 0),
            (true, true, 0),
            (true, false, 1),
            (true, false, 2),
        ] {
            let directory = tempfile::tempdir().unwrap();
            crate::platform::private_dir::ensure_owner_private_directory(directory.path()).unwrap();
            let endpoint = Endpoint::new(directory.path().join("s").to_str().unwrap()).unwrap();
            let listener = Listener::bind_owner_only(&endpoint).unwrap();
            let server = std::thread::spawn(move || {
                serve_connection(listener.accept().unwrap(), &AtomicBool::new(false))
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            let cancelled = AtomicBool::new(false);
            let mut channel =
                Channel::new(Stream::connect_bounded(&endpoint, deadline, &cancelled).unwrap())
                    .unwrap();
            let ready = directory.path().join("ready");
            let spec = LaunchSpec {
                program: std::env::current_exe().unwrap().into_os_string(),
                args: vec![
                    "--exact".into(),
                    "platform_linux::independent_broker::tests::broker_target_fixture".into(),
                    "--ignored".into(),
                ],
                cwd: directory.path().as_os_str().to_owned(),
                environment: vec![("RP_BROKER_READY".into(), ready.as_os_str().to_owned())],
                stdout: None,
                stderr: None,
                readiness: Readiness::File {
                    path: ready.clone().into_os_string(),
                    value: b"ready".to_vec(),
                },
            };
            send(
                &mut channel,
                Body::Launch(encode_spec(&spec)),
                deadline,
                &cancelled,
            )
            .unwrap();
            let Body::Started(pid) = receive(&mut channel, deadline, &cancelled).unwrap() else {
                panic!("expected started target")
            };
            let process = super::super::process_inspect::ProcessLiveness::open_pinned(pid).unwrap();
            assert_eq!(std::fs::read(ready).unwrap(), b"ready");
            if committed {
                send(
                    &mut channel,
                    Body::Commit(wire::Empty {}),
                    deadline,
                    &cancelled,
                )
                .unwrap();
                assert!(matches!(
                    receive(&mut channel, deadline, &cancelled).unwrap(),
                    Body::Committed(_)
                ));
            }
            if stop {
                send(
                    &mut channel,
                    Body::Stop(wire::Empty {}),
                    deadline,
                    &cancelled,
                )
                .unwrap();
                assert!(matches!(
                    receive(&mut channel, deadline, &cancelled).unwrap(),
                    Body::Stopped(_)
                ));
                assert!(!process.is_alive());
            }
            if partial == 1 {
                channel.write_all(&[2]).unwrap();
            }
            if partial == 2 {
                channel.write_all(&[2, 0, 0, 0, 0x32]).unwrap();
            }
            drop(channel);
            if committed && !stop {
                std::thread::sleep(Duration::from_millis(50));
                assert!(
                    process.is_alive(),
                    "committed target must survive requester disconnect"
                );
                process.signal_pinned(libc::SIGKILL).unwrap();
            }
            let result = server.join().unwrap();
            if committed {
                result.unwrap();
            } else {
                assert!(result.is_err());
            }
            assert!(
                !process.is_alive(),
                "broker must clean up uncommitted target"
            );
        }
    }
}
