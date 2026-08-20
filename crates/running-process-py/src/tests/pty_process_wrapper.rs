use super::*;

fn python_process(script: &str) -> NativePtyProcess {
    NativePtyProcess::new(
        vec!["python".into(), "-c".into(), script.into()],
        None,
        None,
        24,
        80,
        None,
    )
    .unwrap()
}

#[test]
fn wrapper_maps_construction_and_prestart_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        assert!(NativePtyProcess::new(vec![], None, None, 24, 80, None).is_err());
        let process = python_process("pass");
        assert_eq!(process.pid().unwrap(), None);
        assert!(!process.terminal_input_relay_active());
        assert_eq!(process.pty_input_bytes_total(), 0);
        assert_eq!(process.pty_newline_events_total(), 0);
        assert_eq!(process.pty_submit_events_total(), 0);
        assert_eq!(process.pty_output_bytes_total(), 0);
        assert_eq!(process.pty_control_churn_bytes_total(), 0);
        assert!(process.write(b"data", false).is_err());
        assert!(process.respond_to_queries(b"query").is_ok());
        assert!(process.resize(30, 100).is_ok());
        assert!(process.send_interrupt().is_err());
        assert!(process.start_terminal_input_relay().is_err());
        process.stop_terminal_input_relay();
        assert!(process.read_chunk(py, Some(0.0)).is_err());
        assert!(process.wait_and_drain(py, Some(0.0), 0.0).is_err());
        let _ = process.wait_for_reader_closed(py, Some(0.0)).unwrap();
        process.close(py).unwrap();
    });
}

#[test]
fn wrapper_drives_real_pty_io_resize_echo_and_wait() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let process = python_process("import time; print('ready', flush=True); time.sleep(30)");
        process.start().unwrap();
        assert!(process.pid().unwrap().is_some());
        assert_eq!(process.poll().unwrap(), None);
        process.resize(32, 120).unwrap();
        process.set_echo(false);
        assert!(!process.echo_enabled());
        process.set_echo(true);
        assert!(process.echo_enabled());
        process.write(b"hello\n", true).unwrap();
        process.respond_to_queries(b"").unwrap();
        assert_eq!(process.pty_input_bytes_total(), 6);
        assert_eq!(process.pty_newline_events_total(), 1);
        assert_eq!(process.pty_submit_events_total(), 1);
        process.terminate(py).unwrap();
        let code = process.wait_and_drain(py, Some(10.0), 2.0).unwrap();
        assert_eq!(process.poll().unwrap(), Some(code));
        let _ = process.wait_for_reader_closed(py, Some(0.0)).unwrap();
        let _ = process.pty_output_bytes_total();
        let _ = process.pty_control_churn_bytes_total();
        process.close(py).unwrap();
    });
}

#[test]
fn wrapper_terminate_and_kill_paths_are_bounded() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let terminate = python_process("import time; time.sleep(30)");
        terminate.start().unwrap();
        terminate.terminate(py).unwrap();
        terminate.wait(Some(5.0)).unwrap();
        terminate.close(py).unwrap();

        let kill = python_process("import time; time.sleep(30)");
        kill.start().unwrap();
        kill.kill(py).unwrap();
        kill.wait(Some(5.0)).unwrap();
        kill.close(py).unwrap();

        let tree_terminate = python_process("import time; time.sleep(30)");
        tree_terminate.start().unwrap();
        tree_terminate.terminate_tree().unwrap();
        tree_terminate.wait(Some(5.0)).unwrap();
        tree_terminate.close(py).unwrap();

        let tree_kill = python_process("import time; time.sleep(30)");
        tree_kill.start().unwrap();
        tree_kill.kill_tree().unwrap();
        tree_kill.wait(Some(5.0)).unwrap();
        tree_kill.close(py).unwrap();
    });
}
