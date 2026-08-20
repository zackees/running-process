use super::*;
use std::ffi::OsString;

fn native(kind: ExactTraceEventKind) -> ExactTraceEvent {
    ExactTraceEvent {
        sequence: 12,
        pid: 42,
        parent_pid: Some(7),
        parent_start_key: Some(70),
        start_key: Some(420),
        timestamp: SystemTime::now(),
        kind,
        executable: Some(PathBuf::from("/usr/bin/worker")),
        argv: Some(vec![OsString::from("worker"), OsString::from("--once")]),
        origin: None,
    }
}

fn event(kind: ProcessEventKind) -> ProcessEvent {
    ProcessEvent {
        kind,
        process: ProcessIdentity {
            pid: 42,
            start_key: Some(420),
        },
        parent: Some(ProcessIdentity {
            pid: 7,
            start_key: Some(70),
        }),
        timestamp: SystemTime::now(),
        executable: Some(PathBuf::from("/usr/bin/worker")),
        argv: Some(vec!["worker".into(), "--once".into()]),
        exit_code: Some(2),
        signal: None,
        raw_exit_status: Some(2),
        backend: "coverage",
        observation_grade: ObservationGrade::ExactTrace,
        coverage_complete: true,
        loss_detected: false,
    }
}

#[test]
fn enums_defaults_and_errors_have_stable_operator_text() {
    assert_eq!(ObservationPolicy::default(), ObservationPolicy::NonInvasive);
    assert_eq!(ObservationPolicy::NonInvasive.as_str(), "non_invasive");
    assert_eq!(ObservationPolicy::AllowTracing.as_str(), "allow_tracing");
    assert_eq!(ObservationPolicy::RequireExact.as_str(), "require_exact");
    assert_eq!(ObservationGrade::ExactTrace.as_str(), "exact_trace");
    assert_eq!(ObservationGrade::ExactEvent.as_str(), "exact_event");
    assert_eq!(
        ObservationGrade::KernelNotification.as_str(),
        "kernel_notification"
    );
    assert_eq!(
        ObservationGrade::KernelHintReconciled.as_str(),
        "kernel_hint_reconciled"
    );
    assert_eq!(
        ObservationGrade::SnapshotInferred.as_str(),
        "snapshot_inferred"
    );
    assert_eq!(
        CaptureSource::RemoteSpawningThread.as_str(),
        "remote_spawning_thread"
    );
    assert_eq!(
        CaptureSource::ManagedSpawnBoundary.as_str(),
        "managed_spawn_boundary"
    );
    assert_eq!(
        CaptureSource::OwnerEventTimeSnapshot.as_str(),
        "owner_event_time_snapshot"
    );
    assert_eq!(CaptureSource::None.as_str(), "none");
    assert_eq!(StackDump::default().capture, StackCapture::OriginPreferred);
    assert_eq!(ProcessObservationError("no".into()).to_string(), "no");
    assert_eq!(
        ProcessWatchConfigurationError("bad".into()).to_string(),
        "bad"
    );
}

#[test]
fn watch_constructors_reject_every_ambiguous_or_unimplemented_shape() {
    assert!(ProcessWatch::on_exec(None, None, None, None, Duration::ZERO, "").is_err());
    assert!(
        ProcessWatch::on_exec(Some(String::new()), None, None, None, Duration::ZERO, "x")
            .unwrap_err()
            .to_string()
            .contains("basename")
    );
    assert!(
        ProcessWatch::on_exit(Some(1), Some(9), None, None, None, Duration::ZERO, "exit",).is_err()
    );
    assert!(ProcessWatch::on_spawn(None, Some(0), Duration::ZERO, "spawn").is_err());
    assert!(ProcessWatch::on_spawn(
        Some(StackDump {
            symbolize_immediately: true,
            ..Default::default()
        }),
        None,
        Duration::ZERO,
        "dump",
    )
    .is_err());
    assert!(ProcessWatch::on_spawn(
        Some(StackDump {
            capture: StackCapture::OwnerAllThreads,
            ..Default::default()
        }),
        None,
        Duration::ZERO,
        "dump",
    )
    .is_err());

    let required = ProcessWatch::on_spawn(
        Some(StackDump {
            capture: StackCapture::OriginRequired,
            ..Default::default()
        }),
        None,
        Duration::ZERO,
        "required",
    )
    .unwrap();
    assert!(required.non_invasive_unsupported_requirement().is_some());
    let failure =
        ProcessWatch::on_failure(Some("worker".into()), None, None, Duration::ZERO, "failure")
            .unwrap();
    assert!(failure.non_invasive_unsupported_requirement().is_some());
}

#[test]
fn selector_matrix_covers_spawn_exec_exit_signal_code_and_failure() {
    let spawn = ProcessWatch::on_spawn(None, None, Duration::ZERO, "spawn").unwrap();
    assert!(selector_matches(
        &spawn.selector,
        &event(ProcessEventKind::Spawn)
    ));
    assert!(!selector_matches(
        &spawn.selector,
        &event(ProcessEventKind::Exec)
    ));

    let by_name = ProcessWatch::on_exec(
        Some("worker".into()),
        None,
        None,
        None,
        Duration::ZERO,
        "exec-name",
    )
    .unwrap();
    let by_path = ProcessWatch::on_exec(
        None,
        Some(PathBuf::from("/usr/bin/worker")),
        None,
        None,
        Duration::ZERO,
        "exec-path",
    )
    .unwrap();
    assert!(selector_matches(
        &by_name.selector,
        &event(ProcessEventKind::Exec)
    ));
    assert!(selector_matches(
        &by_path.selector,
        &event(ProcessEventKind::Exec)
    ));
    let mut wrong = event(ProcessEventKind::Exec);
    wrong.executable = Some(PathBuf::from("/usr/bin/other"));
    assert!(!selector_matches(&by_name.selector, &wrong));
    assert!(!selector_matches(&by_path.selector, &wrong));

    let by_code = ProcessWatch::on_exit(
        Some(2),
        None,
        Some("worker".into()),
        None,
        None,
        Duration::ZERO,
        "code",
    )
    .unwrap();
    assert!(selector_matches(
        &by_code.selector,
        &event(ProcessEventKind::Exit)
    ));
    let by_signal =
        ProcessWatch::on_exit(None, Some(9), None, None, None, Duration::ZERO, "signal").unwrap();
    let mut signaled = event(ProcessEventKind::Exit);
    signaled.exit_code = None;
    signaled.signal = Some(9);
    assert!(selector_matches(&by_signal.selector, &signaled));
    assert!(!selector_matches(
        &by_signal.selector,
        &event(ProcessEventKind::Exit)
    ));

    let failure = ProcessWatch::on_failure(None, None, None, Duration::ZERO, "failure").unwrap();
    assert!(selector_matches(
        &failure.selector,
        &event(ProcessEventKind::Exit)
    ));
    let mut success = event(ProcessEventKind::Exit);
    success.exit_code = Some(0);
    assert!(!selector_matches(&failure.selector, &success));
}

#[test]
fn exact_event_conversion_covers_every_native_kind() {
    let observation = ProcessObservation {
        backend: "trace",
        grade: ObservationGrade::ExactTrace,
        fallback_reason: None,
    };
    let cases = [
        (ExactTraceEventKind::Spawn, ProcessEventKind::Spawn),
        (ExactTraceEventKind::Exec, ProcessEventKind::Exec),
        (
            ExactTraceEventKind::Exit {
                exit_code: Some(3),
                signal: None,
                raw_status: 3,
            },
            ProcessEventKind::Exit,
        ),
        (
            ExactTraceEventKind::Loss {
                reason: "lost".into(),
            },
            ProcessEventKind::Loss,
        ),
    ];
    for (native_kind, expected) in cases {
        let converted = event_from_exact(&native(native_kind), observation.clone(), true);
        assert_eq!(converted.kind, expected);
        assert_eq!(converted.process.pid, 42);
        assert_eq!(converted.parent.as_ref().unwrap().pid, 7);
        assert_eq!(converted.argv.as_ref().unwrap()[1], "--once");
    }
}

#[test]
fn dump_writer_covers_missing_origin_invalid_directory_and_raw_artifact() {
    let owner = write_dump(
        &StackDump {
            capture: StackCapture::OwnerAllThreads,
            ..Default::default()
        },
        "owner",
        &native(ExactTraceEventKind::Spawn),
    );
    assert_eq!(owner.capture_source, CaptureSource::None);
    assert!(owner.error.unwrap().contains("owner all-thread"));

    let missing = write_dump(
        &StackDump {
            capture: StackCapture::OriginRequired,
            ..Default::default()
        },
        "missing",
        &native(ExactTraceEventKind::Spawn),
    );
    assert!(missing.error.unwrap().contains("origin capture"));

    let temp = tempfile::tempdir().unwrap();
    let blocker = temp.path().join("not-a-directory");
    std::fs::write(&blocker, b"file").unwrap();
    let failed = write_dump(
        &StackDump {
            directory: Some(blocker.join("child")),
            ..Default::default()
        },
        "failed",
        &ExactTraceEvent {
            origin: Some(TraceOriginArtifact::default()),
            ..native(ExactTraceEventKind::Spawn)
        },
    );
    assert_eq!(failed.capture_source, CaptureSource::None);
    assert!(failed.error.is_some());

    let origin = TraceOriginArtifact {
        origin_pid: 7,
        thread_id: 8,
        architecture: "x86_64".into(),
        register_format: "fixture".into(),
        executable: Some(PathBuf::from("/usr/bin/worker")),
        registers: vec![0, 1, 254, 255],
        stack_pointer: Some(0x1000),
        instruction_pointer: Some(0x2000),
        stack: vec![2, 3],
        truncated: true,
        module_map: b"map".to_vec(),
        module_map_truncated: false,
    };
    let written = write_dump(
        &StackDump {
            directory: Some(temp.path().to_path_buf()),
            symbolize_immediately: true,
            ..Default::default()
        },
        "bad label!",
        &ExactTraceEvent {
            origin: Some(origin),
            ..native(ExactTraceEventKind::Exec)
        },
    );
    assert_eq!(written.capture_source, CaptureSource::RemoteSpawningThread);
    assert!(written.error.unwrap().contains("deferred"));
    let text = std::fs::read_to_string(&written.artifacts[0]).unwrap();
    assert!(text.contains("registers=0001feff"));
    assert!(written.artifacts[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("bad_label_"));
}

#[test]
fn cursor_reports_timeout_loss_match_and_eof() {
    let watch = ProcessWatch::on_spawn(None, None, Duration::ZERO, "spawn").unwrap();
    let (emitter, subscriber) =
        ProcessWatchEmitter::new(vec![watch.clone()], ObservationPolicy::NonInvasive).unwrap();
    let mut cursor = subscriber.cursor();
    assert!(matches!(
        cursor.read_next(Some(Duration::ZERO)),
        ProcessWatchRead::Timeout
    ));
    push_loss(
        &emitter.log,
        event(ProcessEventKind::Loss),
        "native loss".into(),
    );
    assert!(matches!(
        cursor.read_next(Some(Duration::ZERO)),
        ProcessWatchRead::Loss(_)
    ));
    push_match(&emitter.log, watch, event(ProcessEventKind::Spawn), None);
    assert!(matches!(
        cursor.read_next(Some(Duration::ZERO)),
        ProcessWatchRead::Match(_)
    ));
    assert_eq!(subscriber.snapshot().len(), 1);
    emitter.close();
    assert!(matches!(
        cursor.read_next(Some(Duration::from_secs(1))),
        ProcessWatchRead::Eof
    ));
}

#[test]
fn pending_overflow_aggregates_distinct_native_loss_reasons() {
    let first = PendingDelivery::Loss {
        event: event(ProcessEventKind::Loss),
        reason: "one".into(),
    };
    let mut overflow = first.into_overflow();
    PendingDelivery::Loss {
        event: event(ProcessEventKind::Loss),
        reason: "two".into(),
    }
    .merge_into(&mut overflow);
    PendingDelivery::Loss {
        event: event(ProcessEventKind::Loss),
        reason: "two".into(),
    }
    .merge_into(&mut overflow);
    let watch = ProcessWatch::on_spawn(None, None, Duration::ZERO, "spawn").unwrap();
    PendingDelivery::Match(Box::new(PendingMatch {
        watch,
        event: event(ProcessEventKind::Spawn),
        dump_request: None,
        native: native(ExactTraceEventKind::Spawn),
    }))
    .merge_into(&mut overflow);
    assert_eq!(overflow.additional_dropped, 3);
    assert_eq!(overflow.native_loss_reasons, vec!["one", "two"]);
}
