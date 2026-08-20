use super::*;
use crate::daemon::telemetry::{TeeBackpressure, TeeFileMode};
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::mpsc::Receiver as StdReceiver;
use std::time::Instant;

fn testbin_path(name: &str) -> PathBuf {
    let executable = std::env::current_exe().unwrap();
    let profile = executable
        .parent()
        .and_then(Path::parent)
        .expect("test binary must live under <profile>/deps");
    let path = profile.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "missing fixture {}; run `soldr cargo build -p testbins` first",
        path.display()
    );
    path
}

struct SessionGuard {
    registry: Arc<PipeSessionRegistry>,
    session: Arc<OwnedPipeSession>,
}

impl Deref for SessionGuard {
    type Target = OwnedPipeSession;

    fn deref(&self) -> &Self::Target {
        &self.session
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.session.signal_shutdown();
        let _ = self.session.terminate(Duration::ZERO);
        if self
            .session
            .process
            .wait(Some(Duration::from_secs(2)))
            .is_err()
        {
            let _ = self.session.process.kill();
            let _ = self.session.process.wait(Some(Duration::from_secs(2)));
        }
        self.registry.remove(&self.session.id);
    }
}

fn spawn_scripted(
    registry: Arc<PipeSessionRegistry>,
    merge_stderr_into_stdout: bool,
) -> SessionGuard {
    let fixture = testbin_path("testbin-stdio-scripted");
    let session = registry
        .spawn(
            vec![
                fixture.to_string_lossy().into_owned(),
                "sleep-ms:200".into(),
                "out:stdout-line\n".into(),
                "err:stderr-line\n".into(),
                "echo".into(),
            ],
            None,
            None,
            "coverage-origin".into(),
            "stdio-scripted".into(),
            merge_stderr_into_stdout,
        )
        .unwrap();
    SessionGuard { registry, session }
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(Instant::now() < deadline, "condition did not become true");
        thread::sleep(Duration::from_millis(20));
    }
}

fn event_bytes(receiver: &StdReceiver<TeeEvent>) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(TeeEvent::Bytes(bytes)) => return bytes,
            Ok(TeeEvent::MissedBytes(_)) => {}
            Err(error) => assert!(Instant::now() < deadline, "tee receive failed: {error}"),
        }
    }
}

#[test]
fn spawn_errors_registry_defaults_and_display_are_stable() {
    let registry = Arc::new(PipeSessionRegistry::default());
    let empty = match registry.spawn(
        Vec::new(),
        None,
        None,
        "origin".into(),
        "empty".into(),
        false,
    ) {
        Err(error) => error,
        Ok(_) => panic!("empty argv unexpectedly spawned"),
    };
    assert_eq!(empty.to_string(), "argv must not be empty");

    let missing = match registry.spawn(
        vec!["definitely-not-a-running-process-fixture".into()],
        None,
        None,
        "origin".into(),
        "missing".into(),
        false,
    ) {
        Err(error) => error,
        Ok(_) => panic!("missing executable unexpectedly spawned"),
    };
    assert!(missing
        .to_string()
        .starts_with("failed to spawn pipe session:"));
    assert!(registry.get("missing").is_none());
    assert!(registry.list().is_empty());
    assert!(registry.remove("missing").is_none());
    assert_eq!(registry.purge_exited(""), 0);
}

#[test]
fn live_session_covers_attachments_stream_and_input_tees_and_purge() {
    let temp = tempfile::tempdir().unwrap();
    let stdout_file = temp.path().join("stdout.log");
    let input_file = temp.path().join("stdin.log");
    let registry = Arc::new(PipeSessionRegistry::new());
    let session = spawn_scripted(Arc::clone(&registry), false);
    assert!(session.pid > 0);
    assert_eq!(registry.get(&session.id).unwrap().pid, session.pid);
    assert_eq!(registry.list().len(), 1);
    assert!(session.stream_available(PipeStreamSelect::Stdout));
    assert!(session.stream_available(PipeStreamSelect::Stderr));
    assert!(!session.is_attached(PipeStreamSelect::Stdout));

    let (mut first, _, _) = session
        .attach_stream(PipeStreamSelect::Stdout, false)
        .unwrap();
    assert!(session.is_attached(PipeStreamSelect::Stdout));
    assert!(matches!(
        session.attach_stream(PipeStreamSelect::Stdout, false),
        Err(PipeAttachError::AlreadyAttached)
    ));
    let (_second, _, _) = session
        .attach_stream(PipeStreamSelect::Stdout, true)
        .unwrap();
    assert!(matches!(
        first.receiver.try_recv(),
        Ok(OutboundFrame::Ended(AttachmentEnded::Stolen))
    ));
    session.clear_attachment(PipeStreamSelect::Stdout);
    assert!(!session.is_attached(PipeStreamSelect::Stdout));

    let stdout_ring = session
        .tee_stream_ring(PipeStreamSelect::Stdout, 256)
        .unwrap();
    let stderr_ring = session
        .tee_stream_ring(PipeStreamSelect::Stderr, 256)
        .unwrap();
    let (stdout_channel, stdout_rx) = session
        .tee_stream_channel(PipeStreamSelect::Stdout, 8)
        .unwrap();
    let (stderr_channel, stderr_rx) = session
        .tee_stream_channel_with_options(
            PipeStreamSelect::Stderr,
            8,
            TeeOptions {
                backpressure: TeeBackpressure::Block,
            },
        )
        .unwrap();
    let callback_events = Arc::new(Mutex::new(Vec::new()));
    let callback_copy = Arc::clone(&callback_events);
    let stdout_callback = session
        .tee_stream_callback(PipeStreamSelect::Stdout, 8, move |event| {
            callback_copy.lock().unwrap().push(event);
        })
        .unwrap();
    let callback_copy = Arc::clone(&callback_events);
    let stderr_callback = session
        .tee_stream_callback_with_options(
            PipeStreamSelect::Stderr,
            8,
            TeeOptions {
                backpressure: TeeBackpressure::Block,
            },
            move |event| callback_copy.lock().unwrap().push(event),
        )
        .unwrap();
    let stdout_file_handle = session
        .tee_stream_file(
            PipeStreamSelect::Stdout,
            &stdout_file,
            TeeFileOptions {
                mode: TeeFileMode::Truncate,
                ..Default::default()
            },
        )
        .unwrap();

    let input_ring = session.tee_input_ring(256);
    let (input_channel, input_rx) = session.tee_input_channel(8);
    let (input_blocking, input_blocking_rx) = session.tee_input_channel_with_options(
        8,
        TeeOptions {
            backpressure: TeeBackpressure::Block,
        },
    );
    let input_callbacks = Arc::new(Mutex::new(Vec::new()));
    let callback_copy = Arc::clone(&input_callbacks);
    let input_callback = session.tee_input_callback(8, move |event| {
        callback_copy.lock().unwrap().push(event);
    });
    let callback_copy = Arc::clone(&input_callbacks);
    let input_block_callback = session.tee_input_callback_with_options(
        8,
        TeeOptions {
            backpressure: TeeBackpressure::Block,
        },
        move |event| callback_copy.lock().unwrap().push(event),
    );
    let input_file_handle = session
        .tee_input_file(
            &input_file,
            TeeFileOptions {
                mode: TeeFileMode::Truncate,
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(session.write_stdin(b"input-line\n", true).unwrap(), 11);
    assert!(matches!(
        session.write_stdin(b"again", false),
        Err(ProcessError::StdinUnavailable)
    ));
    assert_eq!(event_bytes(&input_rx), b"input-line\n");
    assert_eq!(event_bytes(&input_blocking_rx), b"input-line\n");
    assert!(event_bytes(&stdout_rx).starts_with(b"stdout-line"));
    assert!(event_bytes(&stderr_rx).starts_with(b"stderr-line"));

    wait_until(|| session.exit_state().is_some());
    wait_until(|| callback_events.lock().unwrap().len() >= 2);
    wait_until(|| input_callbacks.lock().unwrap().len() >= 2);
    wait_until(|| {
        std::fs::read(&stdout_file)
            .unwrap_or_default()
            .contains(&b's')
    });
    wait_until(|| {
        std::fs::read(&input_file)
            .unwrap_or_default()
            .contains(&b'i')
    });

    assert!(session
        .backlog_snapshot(PipeStreamSelect::Stdout)
        .0
        .windows(b"input-line".len())
        .any(|part| part == b"input-line"));
    assert_eq!(
        session.tee_snapshot(stdout_ring).unwrap().stream,
        TeeStream::Stdout
    );
    assert_eq!(
        session.tee_snapshot(stderr_ring).unwrap().stream,
        TeeStream::Stderr
    );
    assert_eq!(
        session.tee_snapshot(input_ring).unwrap().stream,
        TeeStream::Stdin
    );
    for handle in [
        stdout_channel,
        stderr_channel,
        stdout_callback,
        stderr_callback,
        input_channel,
        input_blocking,
        input_callback,
        input_block_callback,
        stdout_file_handle,
        input_file_handle,
    ] {
        assert!(session.tee_status(handle).is_some());
        assert!(session.untee(handle));
    }
    assert!(!session.untee(TeeHandle::from_u64(u64::MAX)));
    assert!(matches!(
        session.attach_stream(PipeStreamSelect::Stdout, false),
        Err(PipeAttachError::SessionExited(_))
    ));
    assert_eq!(
        session.classify_termination(unix_now()),
        TerminationOutcome::NaturalExit
    );
    assert_eq!(registry.purge_exited("different-origin"), 0);
    assert_eq!(registry.purge_exited("coverage-origin"), 1);
}

#[test]
fn merged_stderr_rejects_every_stderr_sink_and_termination_classifies_paths() {
    let registry = Arc::new(PipeSessionRegistry::new());
    let session = spawn_scripted(Arc::clone(&registry), true);
    assert!(!session.stream_available(PipeStreamSelect::Stderr));
    assert!(matches!(
        session.attach_stream(PipeStreamSelect::Stderr, false),
        Err(PipeAttachError::StreamUnavailable)
    ));
    assert!(matches!(
        session.tee_stream_ring(PipeStreamSelect::Stderr, 8),
        Err(PipeAttachError::StreamUnavailable)
    ));
    assert!(matches!(
        session.tee_stream_channel(PipeStreamSelect::Stderr, 8),
        Err(PipeAttachError::StreamUnavailable)
    ));
    assert!(matches!(
        session.tee_stream_callback(PipeStreamSelect::Stderr, 8, |_| {}),
        Err(PipeAttachError::StreamUnavailable)
    ));
    let temp = tempfile::tempdir().unwrap();
    assert_eq!(
        session
            .tee_stream_file(
                PipeStreamSelect::Stderr,
                temp.path().join("unavailable"),
                TeeFileOptions::default(),
            )
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );

    let now = unix_now();
    *session.pending_termination.lock().unwrap() = Some(PendingTermination {
        started_at_unix: now,
        grace_secs: 1.0,
    });
    assert_eq!(
        session.classify_termination(now + 0.5),
        TerminationOutcome::SoftExit
    );
    assert_eq!(
        session.classify_termination(now + 2.0),
        TerminationOutcome::HardKilled
    );
    session.hard_kill_fired.store(true, Ordering::Release);
    assert_eq!(
        session.classify_termination(now + 0.1),
        TerminationOutcome::HardKilled
    );

    session.notify_attached(PipeStreamSelect::Stdout, OutboundFrame::Output(Vec::new()));
    session.signal_shutdown();
}
