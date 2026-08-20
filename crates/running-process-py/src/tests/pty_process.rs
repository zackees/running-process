use running_process::pty as core_pty;
use running_process::pty::NativePtyProcess as CoreNativePtyProcess;

// ── NativePtyProcess: empty argv errors ──

#[test]
fn pty_process_empty_argv_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let result = CoreNativePtyProcess::new(vec![], None, None, 24, 80, None);
        assert!(result.is_err());
    });
}

// ── NativePtyProcess: start already started errors ──

#[test]
fn pty_process_start_already_started_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(0.1)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        let result = process.start_impl();
        assert!(result.is_err());
        let _ = process.close_impl();
    });
}

// ── Iteration 3: PTY Process Integration Tests ──

#[test]
fn pty_process_pid_none_before_start() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        assert!(process.pid().unwrap().is_none());
    });
}

#[test]
fn pty_process_lifecycle_start_wait_close() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "print('hello')".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert!(process.pid().unwrap().is_some());
        if core_pty::wait_before_close_supported() {
            let code = process.wait_impl(Some(10.0)).unwrap();
            assert_eq!(code, 0);
        }
        assert!(process.close_impl().is_ok());
    });
}

#[test]
fn pty_process_poll_none_while_running() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(5)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert!(
            core_pty::poll_pty_process(&process.handles, &process.returncode)
                .unwrap()
                .is_none()
        );
        let _ = process.close_impl();
    });
}

#[test]
fn pty_process_nonzero_exit_code() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import sys; sys.exit(42)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        if core_pty::wait_before_close_supported() {
            let code = process.wait_impl(Some(10.0)).unwrap();
            assert_eq!(code, 42);
        }
        let _ = process.close_impl();
    });
}

#[test]
fn pty_process_write_before_start_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        assert!(process.write_impl(b"test", false).is_err());
    });
}

#[test]
fn pty_process_input_metrics_tracked() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(2)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert_eq!(process.pty_input_bytes_total(), 0);
        let _ = process.write_impl(b"hello\n", false);
        assert_eq!(process.pty_input_bytes_total(), 6);
        assert_eq!(process.pty_newline_events_total(), 1);
        let _ = process.write_impl(b"x", true);
        assert_eq!(process.pty_submit_events_total(), 1);
        let _ = process.close_impl();
    });
}

#[test]
fn pty_process_resize_while_running() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(2)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert!(process.resize_impl(40, 120).is_ok());
        let _ = process.close_impl();
    });
}

#[test]
fn pty_process_kill_running_process() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(0.1)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert!(process.kill_impl().is_ok());
    });
}

#[test]
fn pty_process_terminate_running_process() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(0.1)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert!(process.terminate_impl().is_ok());
        let _ = process.close_impl();
    });
}

#[test]
fn pty_process_close_already_closed_is_noop() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        let _ = process.wait_impl(Some(10.0));
        let _ = process.close_impl();
        assert!(process.close_impl().is_ok());
    });
}

#[test]
fn pty_process_wait_timeout_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec![
            "python".to_string(),
            "-c".to_string(),
            "import time; time.sleep(10)".to_string(),
        ];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        process.start_impl().unwrap();
        assert!(process.wait_impl(Some(0.1)).is_err());
        let _ = process.close_impl();
    });
}

#[test]
fn pty_process_send_interrupt_before_start_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        assert!(process.send_interrupt_impl().is_err());
    });
}

#[test]
fn pty_process_terminate_before_start_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        assert!(process.terminate_impl().is_err());
    });
}

#[test]
fn pty_process_kill_before_start_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        assert!(process.kill_impl().is_err());
    });
}

// ── NativePtyProcess mark_reader_closed / store_returncode tests ──

#[test]
fn pty_process_close_not_started_is_ok() {
    let process = CoreNativePtyProcess::new(
        vec!["python".into(), "-c".into(), "pass".into()],
        None,
        None,
        24,
        80,
        None,
    )
    .unwrap();
    assert!(process.close_impl().is_ok());
}

#[test]
fn pty_process_send_interrupt_running() {
    let process = CoreNativePtyProcess::new(
        vec![
            "python".into(),
            "-c".into(),
            "import time; time.sleep(5)".into(),
        ],
        None,
        None,
        24,
        80,
        None,
    )
    .unwrap();
    process.start_impl().unwrap();
    assert!(process.send_interrupt_impl().is_ok());
    let _ = process.close_impl();
}

#[test]
fn pty_process_with_cwd() {
    let cwd = std::env::temp_dir().to_string_lossy().into_owned();
    let process = CoreNativePtyProcess::new(
        vec!["python".into(), "-c".into(), "pass".into()],
        Some(cwd),
        None,
        24,
        80,
        None,
    )
    .unwrap();
    process.start_impl().unwrap();
    assert!(process.close_impl().is_ok());
}

#[test]
fn pty_process_with_env() {
    let mut env = std::env::vars().collect::<Vec<_>>();
    env.push(("RP_TEST_PTY".into(), "test_value".into()));
    let process = CoreNativePtyProcess::new(
        vec!["python".into(), "-c".into(), "pass".into()],
        None,
        Some(env),
        24,
        80,
        None,
    )
    .unwrap();
    process.start_impl().unwrap();
    assert!(process.close_impl().is_ok());
}

#[test]
fn pty_process_mark_reader_closed() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        // reader should not be closed initially
        assert!(!process.reader.state.lock().unwrap().closed);
        process.mark_reader_closed();
        assert!(process.reader.state.lock().unwrap().closed);
    });
}

#[test]
fn pty_process_store_returncode_sets_value() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        assert!(process.returncode.lock().unwrap().is_none());
        process.store_returncode(42);
        assert_eq!(*process.returncode.lock().unwrap(), Some(42));
    });
}

#[test]
fn pty_process_record_input_metrics_tracks_data() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
        let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
        assert_eq!(process.pty_input_bytes_total(), 0);
        process.record_input_metrics(b"hello\n", false);
        assert_eq!(process.pty_input_bytes_total(), 6);
        assert_eq!(process.pty_newline_events_total(), 1);
        assert_eq!(process.pty_submit_events_total(), 0);
        process.record_input_metrics(b"\r", true);
        assert_eq!(process.pty_submit_events_total(), 1);
    });
}

#[test]
fn pty_process_terminal_input_relay_not_active_initially() {
    let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
    let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
    assert!(!process.terminal_input_relay_active());
}

#[test]
fn pty_process_stop_terminal_input_relay_noop_when_not_started() {
    let argv = vec!["python".to_string(), "-c".to_string(), "pass".to_string()];
    let process = CoreNativePtyProcess::new(argv, None, None, 24, 80, None).unwrap();
    process.stop_terminal_input_relay_impl();
}
