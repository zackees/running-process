use std::time::Duration;

use running_process_core::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StdinMode, StreamKind,
};

#[test]
fn captures_stdout_and_stderr_separately() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import sys; print('out'); print('err', file=sys.stderr)".into(),
        ]),
        cwd: None,
        env: None,
        capture: true,
        creationflags: None,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });

    process.start().unwrap();
    let code = process.wait(Some(Duration::from_secs(5))).unwrap();

    assert_eq!(code, 0);
    assert!(process.captured_stdout().iter().any(|line| line == b"out"));
    assert!(process.captured_stderr().iter().any(|line| line == b"err"));
}

#[test]
fn stream_reads_report_timeout_then_eof() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import time; time.sleep(0.2); print('ready')".into(),
        ]),
        cwd: None,
        env: None,
        capture: true,
        creationflags: None,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });

    process.start().unwrap();
    assert_eq!(
        process.read_stream(StreamKind::Stdout, Some(Duration::from_millis(10))),
        ReadStatus::Timeout
    );
    assert!(matches!(
        process.read_stream(StreamKind::Stdout, Some(Duration::from_secs(2))),
        ReadStatus::Line(line) if line == b"ready"
    ));
    process.wait(Some(Duration::from_secs(5))).unwrap();
    assert_eq!(
        process.read_stream(StreamKind::Stdout, Some(Duration::from_millis(10))),
        ReadStatus::Eof
    );
}

#[test]
fn normalizes_crlf_and_preserves_invalid_bytes() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import sys; sys.stdout.buffer.write(b'bad:\\xff\\r\\nnext\\rthird\\n'); sys.stdout.flush()"
                .into(),
        ]),
        cwd: None,
        env: None,
        capture: true,
        creationflags: None,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });

    process.start().unwrap();
    let code = process.wait(Some(Duration::from_secs(5))).unwrap();

    assert_eq!(code, 0);
    assert_eq!(
        process.captured_stdout(),
        vec![b"bad:\xff".to_vec(), b"next\rthird".to_vec()]
    );
}

#[test]
fn supports_piped_stdin_filter_execution() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import sys; data = sys.stdin.buffer.read(); sys.stdout.buffer.write(data[::-1])"
                .into(),
        ]),
        cwd: None,
        env: None,
        capture: true,
        creationflags: None,
        stdin_mode: StdinMode::Piped,
        nice: None,
    });

    process.start().unwrap();
    process.write_stdin(b"abc").unwrap();
    let code = process.wait(Some(Duration::from_secs(5))).unwrap();

    assert_eq!(code, 0);
    assert_eq!(process.captured_stdout(), vec![b"cba".to_vec()]);
}

#[test]
#[cfg(not(windows))]
fn applies_positive_nice_before_exec() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import os; print(os.nice(0))".into(),
        ]),
        cwd: None,
        env: None,
        capture: true,
        creationflags: None,
        stdin_mode: StdinMode::Inherit,
        nice: Some(5),
    });

    process.start().unwrap();
    let code = process.wait(Some(Duration::from_secs(5))).unwrap();

    assert_eq!(code, 0);
    let observed = String::from_utf8(process.captured_stdout()[0].clone())
        .unwrap()
        .parse::<i32>()
        .unwrap();
    assert!(observed >= 5);
}
