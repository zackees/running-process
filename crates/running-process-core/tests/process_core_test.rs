use std::env;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use running_process_core::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StdinMode, StreamKind,
};

fn config(command: CommandSpec, capture: bool, stdin_mode: StdinMode, nice: Option<i32>) -> ProcessConfig {
    ProcessConfig {
        command,
        cwd: None,
        env: None,
        capture,
        creationflags: None,
        create_process_group: false,
        stdin_mode,
        nice,
    }
}

#[test]
fn captures_stdout_and_stderr_separately() {
    let process = NativeProcess::new(ProcessConfig {
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import sys; print('out'); print('err', file=sys.stderr)".into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
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
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import time; time.sleep(0.2); print('ready')".into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
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
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import sys; sys.stdout.buffer.write(b'bad:\\xff\\r\\nnext\\rthird\\n'); sys.stdout.flush()"
                    .into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
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
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import sys; data = sys.stdin.buffer.read(); sys.stdout.buffer.write(data[::-1])"
                    .into(),
            ]),
            true,
            StdinMode::Piped,
            None,
        )
    });

    process.start().unwrap();
    process.write_stdin(b"abc").unwrap();
    let code = process.wait(Some(Duration::from_secs(5))).unwrap();

    assert_eq!(code, 0);
    assert_eq!(process.captured_stdout(), vec![b"cba".to_vec()]);
}

#[test]
fn captured_output_can_be_cleared_to_release_memory() {
    let process = NativeProcess::new(ProcessConfig {
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import sys; print('alpha'); print('beta', file=sys.stderr)".into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
    });

    process.start().unwrap();
    let code = process.wait(Some(Duration::from_secs(5))).unwrap();

    assert_eq!(code, 0);
    assert_eq!(process.captured_stream_bytes(StreamKind::Stdout), 5);
    assert_eq!(process.captured_stream_bytes(StreamKind::Stderr), 4);
    assert_eq!(process.captured_combined_bytes(), 9);
    assert_eq!(process.clear_captured_stream(StreamKind::Stdout), 5);
    assert!(process.captured_stdout().is_empty());
    assert_eq!(process.captured_stream_bytes(StreamKind::Stdout), 0);
    assert_eq!(process.clear_captured_combined(), 9);
    assert!(process.captured_combined().is_empty());
    assert_eq!(process.captured_combined_bytes(), 0);
}

#[test]
#[cfg(not(windows))]
fn applies_positive_nice_before_exec() {
    let process = NativeProcess::new(ProcessConfig {
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import os; print(os.nice(0))".into(),
            ]),
            true,
            StdinMode::Inherit,
            Some(5),
        )
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

#[test]
#[cfg(windows)]
fn helper_force_killed_parent_reaps_native_child() {
    if env::var("RUNNING_PROCESS_CORE_HELPER").ok().as_deref() != Some("1") {
        return;
    }

    let process = NativeProcess::new(ProcessConfig {
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import time; time.sleep(30)".into(),
            ]),
            false,
            StdinMode::Inherit,
            None,
        )
    });
    process.start().unwrap();
    println!("CHILD_PID={}", process.pid().unwrap());
    std::io::stdout().flush().unwrap();
    thread::sleep(Duration::from_secs(30));
}

#[test]
#[cfg(windows)]
fn force_killed_parent_reaps_native_child_on_windows() {
    let current_exe = env::current_exe().unwrap();
    let mut owner = Command::new(current_exe)
        .arg("--exact")
        .arg("helper_force_killed_parent_reaps_native_child")
        .arg("--nocapture")
        .env("RUNNING_PROCESS_CORE_HELPER", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let child_pid = {
        let stdout = owner.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            let read = reader.read_line(&mut line).unwrap();
            assert!(read != 0, "helper exited before reporting child pid");
            if line.starts_with("CHILD_PID=") {
                break line
                    .trim()
                    .trim_start_matches("CHILD_PID=")
                    .parse::<u32>()
                    .unwrap();
            }
        }
    };

    owner.kill().unwrap();
    owner.wait().unwrap();

    let probe = format!(
        "import psutil, sys; sys.exit(0 if not psutil.pid_exists({child_pid}) else 1)"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let status = Command::new("python")
            .arg("-c")
            .arg(&probe)
            .status()
            .unwrap();
        if status.success() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "child {child_pid} survived owner death");
        thread::sleep(Duration::from_millis(50));
    }
}
