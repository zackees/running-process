use super::*;
use std::io;

fn detector(
    enabled: bool,
    reset_on_input: bool,
    reset_on_output: bool,
    control_output: bool,
) -> IdleDetectorCore {
    IdleDetectorCore {
        timeout_seconds: 0.01,
        stability_window_seconds: 0.0,
        sample_interval_seconds: 0.001,
        reset_on_input,
        reset_on_output,
        count_control_churn_as_output: control_output,
        enabled: Arc::new(AtomicBool::new(enabled)),
        state: Mutex::new(IdleMonitorState {
            last_reset_at: Instant::now(),
            returncode: None,
            interrupted: false,
        }),
        condvar: Condvar::new(),
    }
}

#[test]
fn idle_detector_covers_activity_enablement_exit_and_timeout_outcomes() {
    let disabled = detector(false, false, false, false);
    disabled.record_input(0);
    disabled.record_input(4);
    disabled.record_output(b"");
    disabled.record_output(b"visible");
    assert!(!disabled.enabled());
    assert_eq!(disabled.wait(Some(0.002)).1, "timeout");
    disabled.set_enabled(true);
    assert!(disabled.enabled());
    disabled.set_enabled(true);
    assert_eq!(disabled.wait(Some(0.1)).1, "idle_timeout");

    let visible = detector(true, true, true, false);
    visible.record_input(1);
    visible.record_output(b"\x1b[31m");
    visible.record_output(b"x");
    visible.mark_exit(7, false);
    assert_eq!(visible.wait(None).1, "process_exit");
    assert_eq!(visible.wait(None).3, Some(7));

    let control = detector(true, true, true, true);
    control.record_output(b"\x1b[0m");
    control.mark_exit(-2, true);
    assert_eq!(control.wait(None).1, "interrupt");
}

struct ScriptedReader {
    steps: VecDeque<io::Result<Vec<u8>>>,
}

impl Read for ScriptedReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        match self.steps.pop_front().unwrap_or(Ok(Vec::new())) {
            Ok(bytes) => {
                output[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            Err(error) => Err(error),
        }
    }
}

#[test]
fn pty_reader_retries_transient_errors_records_metrics_and_closes() {
    let shared = Arc::new(PtyReadShared {
        state: Mutex::new(PtyReadState {
            chunks: VecDeque::new(),
            closed: false,
        }),
        condvar: Condvar::new(),
    });
    let idle = Arc::new(detector(true, false, true, true));
    let output = Arc::new(AtomicUsize::new(0));
    let churn = Arc::new(AtomicUsize::new(0));
    spawn_pty_reader(
        Box::new(ScriptedReader {
            steps: VecDeque::from([
                Err(io::Error::from(io::ErrorKind::Interrupted)),
                Err(io::Error::from(io::ErrorKind::WouldBlock)),
                Ok(b"a\x1b[0m".to_vec()),
                Err(io::Error::other("done")),
            ]),
        }),
        Arc::clone(&shared),
        Arc::new(AtomicBool::new(true)),
        Arc::new(Mutex::new(Some(idle))),
        Arc::clone(&output),
        Arc::clone(&churn),
    );
    let state = shared.state.lock().unwrap();
    assert!(state.closed);
    assert_eq!(state.chunks.front().unwrap(), b"a\x1b[0m");
    assert_eq!(output.load(Ordering::Relaxed), 1);
    assert_eq!(churn.load(Ordering::Relaxed), 4);
}

#[test]
fn helpers_cover_metrics_and_error_rendering() {
    let bytes = Arc::new(AtomicUsize::new(0));
    let newlines = Arc::new(AtomicUsize::new(0));
    let submits = Arc::new(AtomicUsize::new(0));
    record_pty_input_metrics(&bytes, &newlines, &submits, b"a\n", true);
    record_pty_input_metrics(&bytes, &newlines, &submits, b"b", false);
    assert_eq!(bytes.load(Ordering::Relaxed), 3);
    assert_eq!(newlines.load(Ordering::Relaxed), 1);
    assert_eq!(submits.load(Ordering::Relaxed), 1);

    for error in [
        PtyError::AlreadyStarted,
        PtyError::NotRunning,
        PtyError::Timeout,
        PtyError::Spawn("spawn".into()),
        PtyError::Other("other".into()),
        PtyError::Io(io::Error::other("io")),
    ] {
        assert!(!error.to_string().is_empty());
    }
}
