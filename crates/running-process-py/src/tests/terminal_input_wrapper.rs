use super::*;

fn event(data: &[u8], submit: bool) -> TerminalInputEventRecord {
    TerminalInputEventRecord {
        data: data.to_vec(),
        submit,
        shift: true,
        ctrl: false,
        alt: true,
        virtual_key_code: 13,
        repeat_count: 2,
    }
}

fn enqueue(input: &NativeTerminalInput, values: &[(&[u8], bool)]) {
    let mut state = input.inner.state.lock().unwrap();
    state.closed = false;
    state
        .events
        .extend(values.iter().map(|(data, submit)| event(data, *submit)));
    input.inner.condvar.notify_all();
}

#[test]
fn event_getters_and_python_data_cover_the_complete_payload() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let event = NativeTerminalInputEvent {
            data: b"enter".to_vec(),
            submit: true,
            shift: true,
            ctrl: false,
            alt: true,
            virtual_key_code: 13,
            repeat_count: 2,
        };
        let _ = event.data(py);
        assert!(event.submit());
        assert!(event.shift());
        assert!(!event.ctrl());
        assert!(event.alt());
        assert_eq!(event.virtual_key_code(), 13);
        assert_eq!(event.repeat_count(), 2);
        assert!(event.__repr__().contains("submit=true"));
    });
}

#[test]
fn queue_read_drain_and_batch_wrappers_cover_each_python_shape() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let input = NativeTerminalInput::new();
        assert!(!input.available());
        assert!(!input.capturing());
        assert_eq!(input.original_console_mode(), None);
        assert_eq!(input.active_console_mode(), None);

        enqueue(&input, &[(b"event", false)]);
        assert!(input.read_event_non_blocking(py).unwrap().is_some());
        enqueue(&input, &[(b"bytes", false)]);
        assert!(input.read_non_blocking(py).unwrap().is_some());

        enqueue(&input, &[(b"a", false), (b"b", true)]);
        assert_eq!(input.drain(py).len(), 2);
        enqueue(&input, &[(b"a", false), (b"b", true)]);
        assert_eq!(input.drain_events(py).unwrap().len(), 2);

        enqueue(&input, &[(b"blocking-event", false)]);
        assert!(input.read_event(py, Some(0.1)).is_ok());
        enqueue(&input, &[(b"blocking-bytes", false)]);
        assert!(input.read(py, Some(0.1)).is_ok());
        enqueue(&input, &[(b"first", false), (b"second", true)]);
        let (_, submit) = input.read_batch(py, Some(0.1)).unwrap();
        assert!(submit);

        input.stop(py).unwrap();
        input.close(py).unwrap();
        assert!(input.read_non_blocking(py).is_err());
        assert!(input.read_event_non_blocking(py).is_err());
    });
}

#[test]
fn empty_open_queue_reports_timeout() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let input = NativeTerminalInput::new();
        input.inner.state.lock().unwrap().closed = false;
        let error = input.read_event(py, Some(0.0)).unwrap_err();
        assert!(error.is_instance_of::<PyTimeoutError>(py));
    });
}
