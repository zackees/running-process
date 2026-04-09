use std::time::Duration;

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
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StdinMode, StreamKind,
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

    let probe =
        format!("import psutil, sys; sys.exit(0 if not psutil.pid_exists({child_pid}) else 1)");
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
                "import time; time.sleep(30)".into(),
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
    let probe = format!("import psutil, sys; sys.exit(0 if psutil.pid_exists({pid}) else 1)");
    Command::new("python")
        .arg("-c")
        .arg(&probe)
        .status()
        .unwrap()
        .success()
}
