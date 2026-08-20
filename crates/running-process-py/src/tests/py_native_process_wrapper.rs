use super::*;

fn pty_process(script: &str) -> PyNativeProcess {
    PyNativeProcess::for_pty(
        vec!["python".into(), "-c".into(), script.into()],
        None,
        None,
        24,
        80,
        None,
    )
    .unwrap()
}

fn running_process(py: Python<'_>, script: &str) -> PyNativeProcess {
    let command = pyo3::types::PyList::new(py, ["python", "-c", script]).unwrap();
    PyNativeProcess::new(
        command.as_any(),
        None,
        false,
        true,
        None,
        None,
        true,
        None,
        None,
        "piped",
        "pipe",
        None,
        false,
        None,
        None,
        "non_invasive",
    )
    .unwrap()
}

#[test]
fn pty_dispatch_covers_prestart_and_backend_specific_contracts() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        assert!(validate_timeout(None).is_ok());
        assert!(validate_timeout(Some(0.0)).is_ok());
        assert!(validate_timeout(Some(-1.0)).is_err());
        assert!(validate_timeout(Some(f64::NAN)).is_err());
        assert!(validate_timeout(Some(f64::INFINITY)).is_err());

        let process = pty_process("pass");
        assert!(process.running_arc().is_err());
        assert!(process.process_observation(py).unwrap().is_none());
        assert!(process.process_watch_snapshot(py).unwrap().is_empty());
        assert_eq!(process.open_process_watch_cursor(), None);
        assert!(process.start_async(py).is_err());
        assert!(process.wait_async(py, None).is_err());
        assert!(process.wait_async(py, Some(-1.0)).is_err());
        assert!(process.kill_async(py).is_err());
        assert!(process.terminate_async(py).is_err());
        assert!(process.close_async(py).is_err());
        assert!(process.write_stdin_async(py, b"x".to_vec()).is_err());
        assert!(process.close_stdin_async(py).is_err());
        assert!(process.poll_async(py).is_err());
        assert!(process.output_async(py).is_err());
        assert!(process.terminate_group_soft_async(py).is_err());
        assert!(process.kill_tree_async(py, 1.0).is_err());
        assert!(process.kill_tree_async(py, -1.0).is_err());
        assert!(process.take_process_watch_match_async(py, 1, None).is_err());
        assert!(process
            .take_process_watch_match_async(py, 1, Some(-1.0))
            .is_err());
        assert!(process.take_process_watch_match(py, 1, Some(0.0)).is_ok());

        assert!(!process.has_pending_combined().unwrap());
        assert!(!process.has_pending_stream("invalid").unwrap());
        assert!(process.drain_combined(py).unwrap().is_empty());
        assert!(process.drain_stream(py, "invalid").unwrap().is_empty());
        assert_eq!(process.take_combined_line(py, Some(0.0)).unwrap().0, "eof");
        assert_eq!(
            process
                .take_stream_line(py, "invalid", Some(0.0))
                .unwrap()
                .0,
            "eof"
        );
        assert!(process.captured_stdout(py).unwrap().is_empty());
        assert!(process.captured_stderr(py).unwrap().is_empty());
        assert!(process.captured_combined(py).unwrap().is_empty());
        assert_eq!(process.captured_stream_bytes("invalid").unwrap(), 0);
        assert_eq!(process.captured_combined_bytes().unwrap(), 0);
        assert_eq!(process.clear_captured_stream("invalid").unwrap(), 0);
        assert_eq!(process.clear_captured_combined().unwrap(), 0);
        assert!(process.write_stdin(b"x").is_err());
        assert!(process.write(b"x", true).is_err());
        assert!(process.read_chunk(py, Some(0.0)).is_err());
        let _ = process.wait_for_pty_reader_closed(py, Some(0.0)).unwrap();
        process.respond_to_queries(b"").unwrap();
        process.resize(30, 100).unwrap();
        assert!(process.send_interrupt().is_err());
        assert!(process.expect(py, "stdout", "x", false, Some(0.0)).is_err());
        assert!(process.start_terminal_input_relay().is_err());
        process.stop_terminal_input_relay().unwrap();
        assert!(!process.terminal_input_relay_active().unwrap());
        let _ = process.pty_input_bytes_total().unwrap();
        let _ = process.pty_newline_events_total().unwrap();
        let _ = process.pty_submit_events_total().unwrap();
        assert_eq!(process.pid().unwrap(), None);
        assert_eq!(process.returncode().unwrap(), None);
        assert!(process.is_pty());
        assert!(process.wait_and_drain(py, Some(0.0), 0.0).is_err());
        process.close(py).unwrap();
    });
}

#[test]
fn pty_dispatch_covers_live_lifecycle_variants() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let terminate = pty_process("import time; time.sleep(30)");
        terminate.start().unwrap();
        assert!(terminate.pid().unwrap().is_some());
        assert_eq!(terminate.poll().unwrap(), None);
        terminate.write(b"hello\n", true).unwrap();
        terminate.terminate(py).unwrap();
        let code = terminate.wait(py, Some(5.0)).unwrap();
        assert_eq!(terminate.returncode().unwrap(), Some(code));
        terminate.close(py).unwrap();

        let kill = pty_process("import time; time.sleep(30)");
        kill.start().unwrap();
        kill.kill(py).unwrap();
        kill.wait(py, Some(5.0)).unwrap();
        kill.close(py).unwrap();

        let terminate_group = pty_process("import time; time.sleep(30)");
        terminate_group.start().unwrap();
        terminate_group.terminate_group().unwrap();
        terminate_group.wait(py, Some(5.0)).unwrap();
        terminate_group.close(py).unwrap();

        let kill_group = pty_process("import time; time.sleep(30)");
        kill_group.start().unwrap();
        kill_group.kill_group().unwrap();
        kill_group.wait(py, Some(5.0)).unwrap();
        kill_group.close(py).unwrap();
    });
}

#[test]
fn running_dispatch_covers_pipe_and_pty_only_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        PyNativeProcess::process_observation_capabilities(py).unwrap();
        let process = running_process(py, "print('native-wrapper', flush=True)");
        assert!(process.running_arc().is_ok());
        let _ = process.process_observation(py).unwrap();
        assert!(process.process_watch_snapshot(py).unwrap().is_empty());
        assert_eq!(process.open_process_watch_cursor(), None);
        assert!(!process.is_pty());
        assert_eq!(process.pid().unwrap(), None);
        assert_eq!(process.returncode().unwrap(), None);
        assert!(process.read_chunk(py, Some(0.0)).is_err());
        assert!(process.wait_for_pty_reader_closed(py, Some(0.0)).is_err());
        process.respond_to_queries(b"").unwrap();
        assert!(process.resize(30, 100).is_err());
        assert!(process.send_interrupt().is_err());
        assert!(process.start_terminal_input_relay().is_err());
        assert!(process.stop_terminal_input_relay().is_err());
        assert!(process.terminal_input_relay_active().is_err());
        assert!(process.pty_input_bytes_total().is_err());
        assert!(process.pty_newline_events_total().is_err());
        assert!(process.pty_submit_events_total().is_err());
        assert!(process.wait_and_drain(py, Some(0.0), 0.0).is_err());

        process.start().unwrap();
        assert!(process.pid().unwrap().is_some());
        let code = process.wait(py, Some(10.0)).unwrap();
        assert_eq!(code, 0);
        assert_eq!(process.poll().unwrap(), Some(0));
        assert_eq!(process.returncode().unwrap(), Some(0));
        assert!(!process.captured_stdout(py).unwrap().is_empty());
        let _ = process.captured_stderr(py).unwrap();
        let _ = process.captured_combined(py).unwrap();
        let _ = process.captured_stream_bytes("stdout").unwrap();
        let _ = process.captured_combined_bytes().unwrap();
        let _ = process.has_pending_combined().unwrap();
        let _ = process.has_pending_stream("stdout").unwrap();
        let _ = process.drain_combined(py).unwrap();
        let _ = process.drain_stream(py, "stdout").unwrap();
        let _ = process.clear_captured_stream("stdout").unwrap();
        let _ = process.clear_captured_combined().unwrap();
        process.close(py).unwrap();
    });
}
