use crate::terminal_input::{NativeTerminalInput, NativeTerminalInputEvent};
use running_process::pty::terminal_input::{TerminalInputCore, TerminalInputEventRecord};
// ── NativeTerminalInput tests ──

#[test]
fn terminal_input_new_starts_closed() {
    let input = NativeTerminalInput::new();
    assert!(!input.capturing());
    let state = input.inner.state.lock().unwrap();
    assert!(state.closed);
    assert!(state.events.is_empty());
}

#[test]
fn terminal_input_available_false_when_empty() {
    let input = NativeTerminalInput::new();
    assert!(!input.available());
}

#[test]
fn terminal_input_next_event_none_when_empty() {
    let input = NativeTerminalInput::new();
    assert!(input.inner.next_event().is_none());
}

#[test]
fn terminal_input_inject_and_consume_event() {
    let input = NativeTerminalInput::new();
    {
        let mut state = input.inner.state.lock().unwrap();
        state.events.push_back(TerminalInputEventRecord {
            data: b"test".to_vec(),
            submit: false,
            shift: false,
            ctrl: false,
            alt: false,
            virtual_key_code: 0,
            repeat_count: 1,
        });
    }
    assert!(input.available());
    let event = input.inner.next_event().unwrap();
    assert_eq!(event.data, b"test");
    assert!(!input.available());
}

#[test]
fn terminal_input_start_errors_on_non_windows() {
    if TerminalInputCore::supported() {
        return;
    }
    pyo3::Python::initialize();
    let input = NativeTerminalInput::new();
    let result = input.start();
    assert!(result.is_err());
}

// ── NativeTerminalInputEvent __repr__ ──

#[test]
fn terminal_input_event_repr() {
    let event = NativeTerminalInputEvent {
        data: vec![0x0D],
        submit: true,
        shift: false,
        ctrl: false,
        alt: false,
        virtual_key_code: 13,
        repeat_count: 1,
    };
    let repr = event.__repr__();
    assert!(repr.contains("submit=true"));
    assert!(repr.contains("virtual_key_code=13"));
}

// ── NativeTerminalInput additional tests ──

#[test]
fn terminal_input_inject_multiple_events() {
    let input = NativeTerminalInput::new();
    {
        let mut state = input.inner.state.lock().unwrap();
        for i in 0..5 {
            state.events.push_back(TerminalInputEventRecord {
                data: vec![b'a' + i],
                submit: false,
                shift: false,
                ctrl: false,
                alt: false,
                virtual_key_code: 0,
                repeat_count: 1,
            });
        }
    }
    assert!(input.available());
    let mut count = 0;
    while input.inner.next_event().is_some() {
        count += 1;
    }
    assert_eq!(count, 5);
    assert!(!input.available());
}

#[test]
fn terminal_input_capturing_false_initially() {
    let input = NativeTerminalInput::new();
    assert!(!input.capturing());
}

// ── NativeTerminalInputEvent fields ──

#[test]
fn terminal_input_event_fields() {
    let event = NativeTerminalInputEvent {
        data: vec![0x1B, 0x5B, 0x41],
        submit: false,
        shift: true,
        ctrl: true,
        alt: false,
        virtual_key_code: 38,
        repeat_count: 2,
    };
    assert_eq!(event.data, vec![0x1B, 0x5B, 0x41]);
    assert!(!event.submit);
    assert!(event.shift);
    assert!(event.ctrl);
    assert!(!event.alt);
    assert_eq!(event.virtual_key_code, 38);
    assert_eq!(event.repeat_count, 2);
    // __repr__ should include all flags
    let repr = event.__repr__();
    assert!(repr.contains("shift=true"));
    assert!(repr.contains("ctrl=true"));
    assert!(repr.contains("alt=false"));
}
