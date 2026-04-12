use std::time::{Duration, Instant};

#[cfg(windows)]
use std::env;
#[cfg(windows)]
use std::fs;
#[cfg(windows)]
use std::io::{BufRead, BufReader, Write};
#[cfg(windows)]
use std::path::PathBuf;
#[cfg(windows)]
use std::process::{Command, Stdio};
#[cfg(windows)]
use std::thread;

use running_process_core::{
    CommandSpec, NativeProcess, ProcessConfig, ProcessError, ReadStatus, StderrMode, StdinMode,
    StreamKind,
};

fn config(
    command: CommandSpec,
    capture: bool,
    stdin_mode: StdinMode,
    nice: Option<i32>,
) -> ProcessConfig {
    ProcessConfig {
        command,
        cwd: None,
        env: None,
        capture,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode,
        nice,
        containment: None,
    }
}

#[test]
fn captures_stderr_in_stdout_by_default() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import sys; print('out'); print('err', file=sys.stderr)".into(),
        ]),
        true,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    let code = process.wait(Some(Duration::from_secs(5))).unwrap();

    assert_eq!(code, 0);
    assert!(process.captured_stdout().iter().any(|line| line == b"out"));
    assert!(process.captured_stdout().iter().any(|line| line == b"err"));
    assert!(process.captured_stderr().is_empty());
}

#[test]
fn captures_stdout_and_stderr_separately_when_requested() {
    let process = NativeProcess::new(ProcessConfig {
        stderr_mode: StderrMode::Pipe,
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
        stderr_mode: StderrMode::Pipe,
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

// ── Error path tests ──

#[test]
fn start_twice_returns_already_started() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import time; time.sleep(0.1)".into(),
        ]),
        false,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    assert!(matches!(process.start(), Err(ProcessError::AlreadyStarted)));
    let _ = process.kill();
}

#[test]
fn write_stdin_before_start_returns_not_running() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec!["python".into(), "-c".into(), "pass".into()]),
        false,
        StdinMode::Piped,
        None,
    ));

    assert!(matches!(
        process.write_stdin(b"hello"),
        Err(ProcessError::NotRunning)
    ));
}

#[test]
fn write_stdin_without_piped_returns_stdin_unavailable() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import time; time.sleep(0.1)".into(),
        ]),
        false,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    assert!(matches!(
        process.write_stdin(b"hello"),
        Err(ProcessError::StdinUnavailable)
    ));
    let _ = process.kill();
}

#[test]
fn kill_before_start_returns_not_running() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec!["python".into(), "-c".into(), "pass".into()]),
        false,
        StdinMode::Inherit,
        None,
    ));

    assert!(matches!(process.kill(), Err(ProcessError::NotRunning)));
}

#[test]
fn wait_before_start_returns_not_running() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec!["python".into(), "-c".into(), "pass".into()]),
        false,
        StdinMode::Inherit,
        None,
    ));

    assert!(matches!(
        process.wait(Some(Duration::from_secs(1))),
        Err(ProcessError::NotRunning)
    ));
}

#[test]
fn wait_timeout_returns_timeout_error() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import time; time.sleep(0.1)".into(),
        ]),
        false,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    assert!(matches!(
        process.wait(Some(Duration::from_millis(100))),
        Err(ProcessError::Timeout)
    ));
    let _ = process.kill();
}

// ── Combined stream tests ──

#[test]
fn read_combined_returns_events_from_both_streams() {
    let process = NativeProcess::new(ProcessConfig {
        stderr_mode: StderrMode::Pipe,
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import sys; print('out'); sys.stdout.flush(); print('err', file=sys.stderr); sys.stderr.flush()".into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
    });

    process.start().unwrap();
    process.wait(Some(Duration::from_secs(5))).unwrap();

    let mut events = Vec::new();
    loop {
        match process.read_combined(Some(Duration::from_millis(100))) {
            ReadStatus::Line(event) => events.push(event),
            ReadStatus::Eof => break,
            ReadStatus::Timeout => break,
        }
    }

    assert!(events
        .iter()
        .any(|e| e.stream == StreamKind::Stdout && e.line == b"out"));
    assert!(events
        .iter()
        .any(|e| e.stream == StreamKind::Stderr && e.line == b"err"));
}

#[test]
fn drain_combined_returns_all_pending() {
    let process = NativeProcess::new(ProcessConfig {
        stderr_mode: StderrMode::Pipe,
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import sys; print('a'); print('b', file=sys.stderr)".into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
    });

    process.start().unwrap();
    process.wait(Some(Duration::from_secs(5))).unwrap();
    // Small sleep to let reader threads finish queuing
    std::thread::sleep(Duration::from_millis(50));

    let events = process.drain_combined();
    assert!(events.len() >= 2);
}

#[test]
fn has_pending_combined_reports_correctly() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec!["python".into(), "-c".into(), "print('hello')".into()]),
        true,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    process.wait(Some(Duration::from_secs(5))).unwrap();
    std::thread::sleep(Duration::from_millis(50));

    assert!(process.has_pending_combined());
    process.drain_combined();
    assert!(!process.has_pending_combined());
}

#[test]
fn captured_combined_includes_both_streams() {
    let process = NativeProcess::new(ProcessConfig {
        stderr_mode: StderrMode::Pipe,
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
    process.wait(Some(Duration::from_secs(5))).unwrap();

    let combined = process.captured_combined();
    assert!(combined
        .iter()
        .any(|e| e.stream == StreamKind::Stdout && e.line == b"out"));
    assert!(combined
        .iter()
        .any(|e| e.stream == StreamKind::Stderr && e.line == b"err"));
}

#[test]
fn captured_combined_bytes_and_clear() {
    let process = NativeProcess::new(ProcessConfig {
        stderr_mode: StderrMode::Pipe,
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import sys; print('ab'); print('cd', file=sys.stderr)".into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
    });

    process.start().unwrap();
    process.wait(Some(Duration::from_secs(5))).unwrap();

    assert_eq!(process.captured_combined_bytes(), 4);
    assert_eq!(process.clear_captured_combined(), 4);
    assert_eq!(process.captured_combined_bytes(), 0);
    assert!(process.captured_combined().is_empty());
}

// ── Shell command mode ──

#[test]
fn shell_command_captures_output() {
    let process = NativeProcess::new(config(
        CommandSpec::Shell("echo shell-works".into()),
        true,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    let code = process.wait(Some(Duration::from_secs(5))).unwrap();

    assert_eq!(code, 0);
    let stdout = process.captured_stdout();
    assert!(
        stdout.iter().any(|line| {
            let text = String::from_utf8_lossy(line);
            text.contains("shell-works")
        }),
        "expected 'shell-works' in output, got: {:?}",
        stdout,
    );
}

// ── Configuration: cwd and env ──

#[test]
fn custom_cwd_is_respected() {
    let tmp = std::env::temp_dir();
    let process = NativeProcess::new(ProcessConfig {
        cwd: Some(tmp.clone()),
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import os; print(os.getcwd())".into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
    });

    process.start().unwrap();
    let code = process.wait(Some(Duration::from_secs(5))).unwrap();

    assert_eq!(code, 0);
    let output = String::from_utf8(process.captured_stdout()[0].clone()).unwrap();
    // Canonicalize both for cross-platform comparison
    let expected = std::fs::canonicalize(&tmp).unwrap_or(tmp);
    let actual = std::fs::canonicalize(output.trim()).unwrap_or_else(|_| output.trim().into());
    assert_eq!(actual, expected);
}

#[test]
fn custom_env_is_applied() {
    // env_clear() wipes everything, so we must pass PATH for python to be found
    let mut env_vars = vec![("RP_TEST_VAR".into(), "hello_coverage".into())];
    if let Ok(path) = std::env::var("PATH") {
        env_vars.push(("PATH".into(), path));
    }
    // Python on Windows also needs SystemRoot for proper operation
    #[cfg(windows)]
    if let Ok(root) = std::env::var("SystemRoot") {
        env_vars.push(("SystemRoot".into(), root));
    }

    let process = NativeProcess::new(ProcessConfig {
        env: Some(env_vars),
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import os; print(os.environ.get('RP_TEST_VAR', 'MISSING'))".into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
    });

    process.start().unwrap();
    let code = process.wait(Some(Duration::from_secs(5))).unwrap();

    assert_eq!(code, 0);
    assert_eq!(process.captured_stdout(), vec![b"hello_coverage".to_vec()]);
}

// ── StdinMode::Null ──

#[test]
fn stdin_null_produces_empty_input() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import sys; data=sys.stdin.buffer.read(); print(len(data))".into(),
        ]),
        true,
        StdinMode::Null,
        None,
    ));

    process.start().unwrap();
    let code = process.wait(Some(Duration::from_secs(5))).unwrap();

    assert_eq!(code, 0);
    assert_eq!(process.captured_stdout(), vec![b"0".to_vec()]);
}

// ── poll() ──

#[test]
fn poll_returns_none_while_running_then_exit_code() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import time; time.sleep(0.3)".into(),
        ]),
        false,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    // Process should still be running
    let status = process.poll().unwrap();
    assert!(status.is_none(), "expected None, got {:?}", status);

    // Wait for it to finish
    process.wait(Some(Duration::from_secs(5))).unwrap();
    let status = process.poll().unwrap();
    assert_eq!(status, Some(0));
}

// ── close() and terminate() ──

#[test]
fn close_kills_running_process() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import time; time.sleep(0.1)".into(),
        ]),
        false,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    process.close().unwrap();
}

#[test]
fn close_on_already_finished_is_noop() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec!["python".into(), "-c".into(), "pass".into()]),
        false,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    process.wait(Some(Duration::from_secs(5))).unwrap();
    process.close().unwrap();
}

#[test]
fn terminate_kills_running_process() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import time; time.sleep(0.1)".into(),
        ]),
        false,
        StdinMode::Inherit,
        None,
    ));

    process.start().unwrap();
    process.terminate().unwrap();
}

// ── pid() ──

#[test]
fn pid_returns_some_after_start() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec![
            "python".into(),
            "-c".into(),
            "import time; time.sleep(0.1)".into(),
        ]),
        false,
        StdinMode::Inherit,
        None,
    ));

    assert!(process.pid().is_none());
    process.start().unwrap();
    assert!(process.pid().is_some());
    let _ = process.kill();
}

// ── process group (Unix) ──

#[test]
#[cfg(not(windows))]
fn create_process_group_sets_new_pgid() {
    let process = NativeProcess::new(ProcessConfig {
        create_process_group: true,
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import os; print(os.getpgid(0) == os.getpid())".into(),
            ]),
            true,
            StdinMode::Inherit,
            None,
        )
    });

    process.start().unwrap();
    let code = process.wait(Some(Duration::from_secs(5))).unwrap();

    assert_eq!(code, 0);
    assert_eq!(process.captured_stdout(), vec![b"True".to_vec()]);
}

// ── Windows tests ──

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
                "import time; time.sleep(0.1)".into(),
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

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if !pid_exists(child_pid) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "child {child_pid} survived owner death"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[cfg(windows)]
fn helper_force_killed_parent_logs_native_child() {
    if env::var("RUNNING_PROCESS_CORE_HELPER_LOGGED")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }

    let process = NativeProcess::new(ProcessConfig {
        ..config(
            CommandSpec::Argv(vec![
                "python".into(),
                "-c".into(),
                "import time; time.sleep(0.1)".into(),
            ]),
            false,
            StdinMode::Inherit,
            None,
        )
    });
    process.start().unwrap();
    println!("OWNER_READY");
    std::io::stdout().flush().unwrap();
    thread::sleep(Duration::from_secs(30));
}

#[test]
#[cfg(windows)]
fn repeated_force_killed_parents_leave_no_logged_native_children_on_windows() {
    let current_exe = env::current_exe().unwrap();
    let log_path = unique_pid_log_path();
    let owner_count = 6;
    let mut owners = Vec::new();

    for _ in 0..owner_count {
        let mut owner = Command::new(&current_exe)
            .arg("--exact")
            .arg("helper_force_killed_parent_logs_native_child")
            .arg("--nocapture")
            .env("RUNNING_PROCESS_CORE_HELPER_LOGGED", "1")
            .env("RUNNING_PROCESS_CHILD_PID_LOG_PATH", &log_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        {
            let stdout = owner.stdout.take().unwrap();
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                let read = reader.read_line(&mut line).unwrap();
                assert!(read != 0, "helper exited before reporting readiness");
                if line.trim() == "OWNER_READY" {
                    break;
                }
            }
        }

        owners.push(owner);
    }

    for owner in &mut owners {
        owner.kill().unwrap();
        owner.wait().unwrap();
    }

    let child_pids = read_logged_pids(&log_path);
    assert_eq!(child_pids.len(), owner_count);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let all_dead = child_pids.iter().all(|pid| !pid_exists(*pid));
        if all_dead {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "some logged child pids survived owner death: {child_pids:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }

    let _ = fs::remove_file(&log_path);
}

#[cfg(windows)]
fn unique_pid_log_path() -> PathBuf {
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    env::temp_dir().join(format!("running-process-native-child-pids-{suffix}.log"))
}

#[cfg(windows)]
fn read_logged_pids(path: &PathBuf) -> Vec<u32> {
    let content = fs::read_to_string(path).unwrap();
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| line.parse::<u32>().unwrap())
        .collect()
}

#[cfg(windows)]
fn pid_exists(pid: u32) -> bool {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::processthreadsapi::{GetExitCodeProcess, OpenProcess};
    use winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION;

    const STILL_ACTIVE: u32 = 259;

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }

    let mut exit_code = 0u32;
    let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) } != 0;
    unsafe {
        CloseHandle(handle);
    }
    ok && exit_code == STILL_ACTIVE
}

#[test]
fn returncode_auto_updates_without_poll() {
    let process = NativeProcess::new(config(
        CommandSpec::Argv(vec!["python".into(), "-c".into(), "print('hello')".into()]),
        true,
        StdinMode::Null,
        None,
    ));

    process.start().unwrap();

    // Wait up to 5 seconds for returncode to auto-update via the background waiter thread
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if process.returncode().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(
        process.returncode().is_some(),
        "returncode should auto-update via background waiter thread without calling poll()"
    );
    assert_eq!(process.returncode(), Some(0));
}

