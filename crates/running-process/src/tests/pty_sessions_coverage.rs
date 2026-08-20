use super::*;
use crate::daemon::telemetry::{TeeBackpressure, TeeFileMode};
use std::time::Instant;

fn session() -> Arc<OwnedPtySession> {
    let process = NativePtyProcess::new(vec!["unused".into()], None, None, 24, 80, None).unwrap();
    Arc::new(OwnedPtySession {
        id: "pty-coverage".into(),
        process: Arc::new(process),
        pid: 0,
        command: "unused".into(),
        cwd: String::new(),
        originator: "coverage-origin".into(),
        created_at_unix: unix_now(),
        rows: AtomicU16::new(24),
        cols: AtomicU16::new(80),
        backlog: Mutex::new(RingBuffer::new(64)),
        tees: TeeRegistry::new(),
        observers: ObserverRegistry::new(),
        attached: Mutex::new(None),
        exit_state: Mutex::new(None),
        pending_termination: Mutex::new(None),
        hard_kill_fired: Arc::new(AtomicBool::new(false)),
        reader_shutdown: Arc::new(AtomicBool::new(false)),
        reader_thread: Mutex::new(None),
    })
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "condition did not become true");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn attachment_metadata_steal_backlog_and_exit_paths_are_deterministic() {
    let session = session();
    session.backlog.lock().unwrap().push(b"backlog");
    assert_eq!(session.backlog_snapshot().0, b"backlog");
    assert_eq!(session.rows(), 24);
    assert_eq!(session.cols(), 80);
    assert!(!session.is_attached());
    assert!(!session.attached_is_tty());
    assert!(session.attached_term().is_empty());
    assert_eq!(
        session.attached_graphics_capabilities(),
        TerminalGraphicsCapabilities::unknown()
    );

    let graphics = TerminalGraphicsCapabilities::unknown();
    let (mut first, backlog, dropped) = session
        .attach_with_terminal_info(false, 40, 120, false, "dumb".into(), graphics.clone())
        .unwrap();
    assert_eq!(backlog, b"backlog");
    assert_eq!(dropped, 0);
    assert!(session.is_attached());
    assert!(!session.attached_is_tty());
    assert_eq!(session.attached_term(), "dumb");
    assert_eq!(session.attached_graphics_capabilities(), graphics);
    assert_eq!(session.rows(), 24, "non-TTY attach must not resize");
    assert!(matches!(
        session.attach(false, 30, 90),
        Err(AttachError::AlreadyAttached)
    ));

    let (_second, _, _) = session.attach(true, 0, 0).unwrap();
    assert!(matches!(
        first.receiver.try_recv(),
        Ok(OutboundFrame::Ended(AttachmentEnded::Stolen))
    ));
    session.notify_attached(OutboundFrame::MissedBytes(7));
    session.clear_attachment();
    assert!(!session.is_attached());

    let exited = ExitState {
        exit_code: 9,
        exited_at_unix: unix_now(),
        outcome: TerminationOutcome::NaturalExit,
    };
    *session.exit_state.lock().unwrap() = Some(exited);
    assert!(matches!(
        session.attach(false, 24, 80),
        Err(AttachError::SessionExited(_))
    ));
}

#[test]
fn output_and_input_tee_wrappers_cover_ring_channel_callback_file_and_status() {
    let session = session();
    let temp = tempfile::tempdir().unwrap();
    let output_path = temp.path().join("output.log");
    let input_path = temp.path().join("input.log");

    let output_ring = session.tee_output_ring(64);
    let (output_channel, output_rx) = session.tee_output_channel(4);
    let (output_block, output_block_rx) = session.tee_output_channel_with_options(
        4,
        TeeOptions {
            backpressure: TeeBackpressure::Block,
        },
    );
    let output_callbacks = Arc::new(Mutex::new(Vec::new()));
    let callback_copy = Arc::clone(&output_callbacks);
    let output_callback =
        session.tee_output_callback(4, move |event| callback_copy.lock().unwrap().push(event));
    let callback_copy = Arc::clone(&output_callbacks);
    let output_block_callback = session.tee_output_callback_with_options(
        4,
        TeeOptions {
            backpressure: TeeBackpressure::Block,
        },
        move |event| callback_copy.lock().unwrap().push(event),
    );
    let output_file = session
        .tee_output_file(
            &output_path,
            TeeFileOptions {
                mode: TeeFileMode::Truncate,
                ..Default::default()
            },
        )
        .unwrap();

    let input_ring = session.tee_input_ring(64);
    let (input_channel, input_rx) = session.tee_input_channel(4);
    let (input_block, input_block_rx) = session.tee_input_channel_with_options(
        4,
        TeeOptions {
            backpressure: TeeBackpressure::Block,
        },
    );
    let input_callbacks = Arc::new(Mutex::new(Vec::new()));
    let callback_copy = Arc::clone(&input_callbacks);
    let input_callback =
        session.tee_input_callback(4, move |event| callback_copy.lock().unwrap().push(event));
    let callback_copy = Arc::clone(&input_callbacks);
    let input_block_callback = session.tee_input_callback_with_options(
        4,
        TeeOptions {
            backpressure: TeeBackpressure::Block,
        },
        move |event| callback_copy.lock().unwrap().push(event),
    );
    let input_file = session
        .tee_input_file(
            &input_path,
            TeeFileOptions {
                mode: TeeFileMode::Truncate,
                ..Default::default()
            },
        )
        .unwrap();

    session.tees.write(TeeStream::PtyOutput, b"output");
    session.tees.write(TeeStream::Stdin, b"input");
    assert_eq!(
        output_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        TeeEvent::Bytes(b"output".to_vec())
    );
    assert_eq!(
        output_block_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap(),
        TeeEvent::Bytes(b"output".to_vec())
    );
    assert_eq!(
        input_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        TeeEvent::Bytes(b"input".to_vec())
    );
    assert_eq!(
        input_block_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        TeeEvent::Bytes(b"input".to_vec())
    );
    wait_until(|| output_callbacks.lock().unwrap().len() >= 2);
    wait_until(|| input_callbacks.lock().unwrap().len() >= 2);
    wait_until(|| std::fs::read(&output_path).unwrap_or_default() == b"output");
    wait_until(|| std::fs::read(&input_path).unwrap_or_default() == b"input");

    assert_eq!(
        session.tee_snapshot(output_ring).unwrap().stream,
        TeeStream::PtyOutput
    );
    assert_eq!(
        session.tee_snapshot(input_ring).unwrap().stream,
        TeeStream::Stdin
    );
    for handle in [
        output_channel,
        output_block,
        output_callback,
        output_block_callback,
        output_file,
        input_channel,
        input_block,
        input_callback,
        input_block_callback,
        input_file,
    ] {
        assert!(session.tee_status(handle).is_some());
        assert!(session.untee(handle));
    }
    assert!(!session.untee(TeeHandle::from_u64(u64::MAX)));
}

#[test]
fn unstarted_process_errors_and_termination_classification_cover_control_paths() {
    let session = session();
    assert!(session.write_input(b"input").is_err());
    assert!(session.resize(40, 120).is_ok());
    assert_eq!(session.rows(), 40);
    assert_eq!(session.cols(), 120);
    assert!(session.send_interrupt().is_err());
    assert_eq!(
        session.classify_termination(unix_now()),
        TerminationOutcome::NaturalExit
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
    let termination = session.terminate(Duration::ZERO);
    #[cfg(windows)]
    assert!(termination.is_ok());
    #[cfg(unix)]
    assert!(matches!(termination, Err(crate::pty::PtyError::NotRunning)));
}

#[test]
fn registry_empty_errors_manual_exit_purge_and_display_variants() {
    let registry = Arc::new(PtySessionRegistry::default());
    let empty = match registry.spawn(
        Vec::new(),
        None,
        None,
        24,
        80,
        "origin".into(),
        "empty".into(),
    ) {
        Err(error) => error,
        Ok(_) => panic!("empty argv unexpectedly spawned"),
    };
    assert_eq!(empty.to_string(), "argv must not be empty");
    assert_eq!(
        SpawnError::Construct("bad".into()).to_string(),
        "failed to build PTY process: bad"
    );
    assert_eq!(
        SpawnError::Spawn("bad".into()).to_string(),
        "failed to spawn PTY: bad"
    );
    assert!(registry.get("missing").is_none());
    assert!(registry.list().is_empty());
    assert!(registry.remove("missing").is_none());
    assert_eq!(registry.purge_exited(""), 0);

    let session = session();
    *session.exit_state.lock().unwrap() = Some(ExitState {
        exit_code: 0,
        exited_at_unix: unix_now(),
        outcome: TerminationOutcome::NaturalExit,
    });
    registry
        .sessions
        .lock()
        .unwrap()
        .insert(session.id.clone(), Arc::clone(&session));
    assert!(registry.get(&session.id).is_some());
    assert_eq!(registry.list().len(), 1);
    assert_eq!(registry.purge_exited("other"), 0);
    assert_eq!(registry.purge_exited("coverage-origin"), 1);
}
