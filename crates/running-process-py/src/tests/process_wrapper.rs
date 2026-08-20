use super::*;
use running_process::{
    CaptureSource, DumpResult, ObservationGrade, ProcessEvent, ProcessIdentity, ProcessWatchGap,
    ProcessWatchLoss,
};
use std::time::SystemTime;

fn event(kind: ProcessEventKind) -> ProcessEvent {
    ProcessEvent {
        kind,
        process: ProcessIdentity {
            pid: 42,
            start_key: Some(7),
        },
        parent: Some(ProcessIdentity {
            pid: 21,
            start_key: Some(3),
        }),
        timestamp: SystemTime::UNIX_EPOCH,
        executable: Some(PathBuf::from("python")),
        argv: Some(vec!["python".to_owned(), "-c".to_owned()]),
        exit_code: Some(0),
        signal: None,
        raw_exit_status: Some(0),
        backend: "test",
        observation_grade: ObservationGrade::ExactTrace,
        coverage_complete: true,
        loss_detected: false,
    }
}

fn assert_type(py: Python<'_>, value: &Py<PyAny>, expected: &str) {
    assert_eq!(
        value
            .bind(py)
            .get_item("type")
            .unwrap()
            .extract::<String>()
            .unwrap(),
        expected
    );
}

#[test]
fn parsing_covers_valid_variants_defaults_and_validation_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        assert_eq!(
            parse_observation_policy("non_invasive").unwrap(),
            ObservationPolicy::NonInvasive
        );
        assert_eq!(
            parse_observation_policy("allow_tracing").unwrap(),
            ObservationPolicy::AllowTracing
        );
        assert_eq!(
            parse_observation_policy("require_exact").unwrap(),
            ObservationPolicy::RequireExact
        );
        assert!(parse_observation_policy("invalid").is_err());

        let empty = PyDict::new(py);
        assert_eq!(optional_string(&empty, "missing").unwrap(), None);
        assert_eq!(optional_i32(&empty, "missing").unwrap(), None);
        assert_eq!(optional_limit(&empty).unwrap(), Some(1));
        assert_eq!(parse_stack_dump(&empty).unwrap(), None);
        empty.set_item("limit", py.None()).unwrap();
        assert_eq!(optional_limit(&empty).unwrap(), None);

        let values = PyDict::new(py);
        values.set_item("name", "value").unwrap();
        values.set_item("number", 5).unwrap();
        values.set_item("none", py.None()).unwrap();
        assert_eq!(
            optional_string(&values, "name").unwrap().as_deref(),
            Some("value")
        );
        assert_eq!(optional_string(&values, "none").unwrap(), None);
        assert_eq!(optional_i32(&values, "number").unwrap(), Some(5));
        assert_eq!(optional_i32(&values, "none").unwrap(), None);

        for capture in ["origin_preferred", "origin_required", "owner_all_threads"] {
            let dump = PyDict::new(py);
            dump.set_item("dump_capture", capture).unwrap();
            dump.set_item("dump_symbolize", "immediate").unwrap();
            dump.set_item("dump_directory", ".").unwrap();
            assert!(parse_stack_dump(&dump).unwrap().is_some());
        }
        let invalid_dump = PyDict::new(py);
        invalid_dump.set_item("dump_capture", "invalid").unwrap();
        assert!(parse_stack_dump(&invalid_dump).is_err());
        invalid_dump
            .set_item("dump_capture", "origin_preferred")
            .unwrap();
        invalid_dump.set_item("dump_symbolize", "invalid").unwrap();
        assert!(parse_stack_dump(&invalid_dump).is_err());

        for kind in ["spawn", "exec", "exit", "failure"] {
            let watch = PyDict::new(py);
            watch.set_item("kind", kind).unwrap();
            watch.set_item("label", format!("{kind}-watch")).unwrap();
            watch.set_item("cooldown_seconds", 0.0).unwrap();
            watch.set_item("limit", 2).unwrap();
            if kind == "exec" || kind == "failure" {
                watch.set_item("basename", "python").unwrap();
            }
            if kind == "exit" {
                watch.set_item("code", 0).unwrap();
                watch.set_item("signal", py.None()).unwrap();
            }
            assert!(parse_process_watch(&watch).is_ok(), "kind={kind}");
        }
        assert!(parse_process_watch(&empty).is_err());
        let invalid_watch = PyDict::new(py);
        invalid_watch.set_item("kind", "invalid").unwrap();
        invalid_watch.set_item("label", "invalid").unwrap();
        assert!(parse_process_watch(&invalid_watch).is_err());
        invalid_watch.set_item("kind", "spawn").unwrap();
        invalid_watch.set_item("cooldown_seconds", -1.0).unwrap();
        assert!(parse_process_watch(&invalid_watch).is_err());
    });
}

#[test]
fn event_conversion_covers_every_record_shape() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        assert_eq!(event_kind_name(ProcessEventKind::Spawn), "spawn");
        assert_eq!(event_kind_name(ProcessEventKind::Exec), "exec");
        assert_eq!(event_kind_name(ProcessEventKind::Exit), "exit");
        assert_eq!(event_kind_name(ProcessEventKind::Loss), "loss");

        let watch = ProcessWatch::on_spawn(None, Some(1), Duration::ZERO, "spawn-watch").unwrap();
        let matched = ProcessWatchMatch {
            sequence: 1,
            watch: watch.clone(),
            event: event(ProcessEventKind::Spawn),
            dump: Some(DumpResult {
                capture_source: CaptureSource::ManagedSpawnBoundary,
                artifacts: vec![PathBuf::from("dump.txt")],
                symbolized: true,
                error: None,
            }),
        };
        let matched = watch_read_to_python(py, ProcessWatchRead::Match(Box::new(matched))).unwrap();
        assert_type(py, &matched, "match");
        let matched = matched.bind(py);
        assert_eq!(
            matched
                .get_item("sequence")
                .unwrap()
                .extract::<u64>()
                .unwrap(),
            1
        );
        assert_eq!(
            matched
                .get_item("watch_label")
                .unwrap()
                .extract::<String>()
                .unwrap(),
            "spawn-watch"
        );
        let matched_event = matched.get_item("event").unwrap();
        assert_eq!(
            matched_event
                .get_item("kind")
                .unwrap()
                .extract::<String>()
                .unwrap(),
            "spawn"
        );
        assert_eq!(
            matched_event
                .get_item("pid")
                .unwrap()
                .extract::<u32>()
                .unwrap(),
            42
        );
        let dump = matched.get_item("dump").unwrap();
        assert_eq!(
            dump.get_item("capture_source")
                .unwrap()
                .extract::<String>()
                .unwrap(),
            "managed_spawn_boundary"
        );
        assert!(dump
            .get_item("symbolized")
            .unwrap()
            .extract::<bool>()
            .unwrap());
        assert!(dump.get_item("error").unwrap().is_none());

        let without_dump = ProcessWatchMatch {
            sequence: 2,
            watch,
            event: event(ProcessEventKind::Exec),
            dump: None,
        };
        let without_dump = watch_match_to_python(py, &without_dump).unwrap();
        assert_type(py, &without_dump, "match");
        assert_eq!(
            without_dump
                .bind(py)
                .get_item("sequence")
                .unwrap()
                .extract::<u64>()
                .unwrap(),
            2
        );
        assert!(without_dump.bind(py).get_item("dump").unwrap().is_none());

        let loss = ProcessWatchLoss {
            sequence: 3,
            event: event(ProcessEventKind::Loss),
            reason: "test loss".to_owned(),
        };
        let loss = watch_read_to_python(py, ProcessWatchRead::Loss(Box::new(loss))).unwrap();
        assert_type(py, &loss, "loss");
        assert_eq!(
            loss.bind(py)
                .get_item("sequence")
                .unwrap()
                .extract::<u64>()
                .unwrap(),
            3
        );
        assert_eq!(
            loss.bind(py)
                .get_item("reason")
                .unwrap()
                .extract::<String>()
                .unwrap(),
            "test loss"
        );
        assert_eq!(
            loss.bind(py)
                .get_item("event")
                .unwrap()
                .get_item("kind")
                .unwrap()
                .extract::<String>()
                .unwrap(),
            "loss"
        );

        let gap = watch_read_to_python(
            py,
            ProcessWatchRead::Gap(ProcessWatchGap {
                first_missing: 4,
                last_missing: 8,
            }),
        )
        .unwrap();
        assert_type(py, &gap, "gap");
        assert_eq!(
            gap.bind(py)
                .get_item("first_missing")
                .unwrap()
                .extract::<u64>()
                .unwrap(),
            4
        );
        assert_eq!(
            gap.bind(py)
                .get_item("last_missing")
                .unwrap()
                .extract::<u64>()
                .unwrap(),
            8
        );

        let timeout = watch_read_to_python(py, ProcessWatchRead::Timeout).unwrap();
        assert_type(py, &timeout, "timeout");
        let eof = watch_read_to_python(py, ProcessWatchRead::Eof).unwrap();
        assert_type(py, &eof, "eof");
    });
}
