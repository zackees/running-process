//! Integration tests for `RUNNING_PROCESS_ORIGINATOR` env var propagation and
//! `find_processes_by_originator` scanner.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use running_process_core::originator::find_processes_by_originator;
use running_process_core::ContainedProcessGroup;

/// Build and locate a test binary from the workspace.
fn testbin_path(name: &str) -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .args(["build", "-p", name, "--message-format=json"])
        .stderr(std::process::Stdio::inherit())
        .output()
        .expect("failed to run cargo build");
    assert!(
        output.status.success(),
        "`cargo build -p {name}` failed with status {}",
        output.status,
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if !line.contains("\"compiler-artifact\"") || !line.contains(name) {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v["reason"] == "compiler-artifact"
                && v["target"]["kind"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|k| k == "bin"))
            {
                if let Some(exe) = v["executable"].as_str() {
                    let p = PathBuf::from(exe);
                    assert!(p.exists(), "cargo reported {p:?} but it does not exist");
                    return p;
                }
            }
        }
    }

    panic!("`cargo build -p {name}` succeeded but no binary artifact found in JSON output");
}

#[cfg(windows)]
fn force_kill(pid: u32) {
    unsafe {
        let handle = winapi::um::processthreadsapi::OpenProcess(
            winapi::um::winnt::PROCESS_TERMINATE,
            0,
            pid,
        );
        if !handle.is_null() {
            winapi::um::processthreadsapi::TerminateProcess(handle, 1);
            winapi::um::handleapi::CloseHandle(handle);
        }
    }
}

#[cfg(unix)]
fn force_kill(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

fn read_until_ready(
    child: &mut running_process_core::ContainedChild,
) -> (Option<u32>, Option<String>) {
    let stdout = child.child.stdout.take().expect("stdout");
    let reader = BufReader::new(stdout);

    let mut pid: Option<u32> = None;
    let mut originator: Option<String> = None;

    let start = Instant::now();
    for line in reader.lines() {
        if start.elapsed() > Duration::from_secs(10) {
            panic!("timed out reading env-reporter output");
        }
        let line = line.expect("read line");
        if let Some(val) = line.strip_prefix("PID=") {
            pid = Some(val.trim().parse().expect("parse PID"));
        } else if let Some(val) = line.strip_prefix("ORIGINATOR=") {
            originator = Some(val.trim().to_string());
        } else if line.trim() == "READY" {
            break;
        }
    }

    (pid, originator)
}

#[test]
fn test_originator_env_var_is_set_on_child() {
    let env_reporter = testbin_path("testbin-env-reporter");
    let group = ContainedProcessGroup::with_originator("TESTOOL").expect("create group");

    let mut cmd = Command::new(&env_reporter);
    cmd.stdout(std::process::Stdio::piped());
    let mut child = group.spawn(&mut cmd).expect("spawn");

    let (child_pid, originator) = read_until_ready(&mut child);
    assert!(child_pid.is_some(), "should get child PID");

    let originator = originator.expect("should get originator value");
    let expected = format!("TESTOOL:{}", std::process::id());
    assert_eq!(originator, expected);

    drop(group);
    if let Some(pid) = child_pid {
        force_kill(pid);
    }
}

#[test]
fn test_no_originator_env_var_without_originator() {
    let env_reporter = testbin_path("testbin-env-reporter");
    let group = ContainedProcessGroup::new().expect("create group");

    let mut cmd = Command::new(&env_reporter);
    cmd.stdout(std::process::Stdio::piped());
    let mut child = group.spawn(&mut cmd).expect("spawn");

    let (child_pid, originator) = read_until_ready(&mut child);
    assert!(child_pid.is_some(), "should get child PID");

    let originator = originator.expect("should get originator line");
    assert_eq!(originator, "<not set>");

    drop(group);
    if let Some(pid) = child_pid {
        force_kill(pid);
    }
}

#[test]
fn test_find_processes_by_originator_finds_child() {
    let sleeper = testbin_path("testbin-sleeper");

    let tool_name = format!("TESTFIND{}", std::process::id());
    let group = ContainedProcessGroup::with_originator(&tool_name).expect("create group");

    let mut cmd = Command::new(&sleeper);
    cmd.stdout(std::process::Stdio::piped());
    let child = group.spawn(&mut cmd).expect("spawn");
    let child_pid = child.child.id();

    std::thread::sleep(Duration::from_millis(500));

    let results = find_processes_by_originator(&tool_name);

    let found = results.iter().any(|r| r.pid == child_pid);
    assert!(
        found,
        "should find child PID {child_pid} in scan results; found {} results",
        results.len(),
    );

    for r in &results {
        if r.pid == child_pid {
            assert!(r.parent_alive, "parent should be alive");
            assert_eq!(r.parent_pid, std::process::id());
        }
    }

    drop(group);
    force_kill(child_pid);
}

#[test]
fn test_find_processes_excludes_non_matching_tool() {
    let sleeper = testbin_path("testbin-sleeper");

    let tool_name = format!("EXCL{}", std::process::id());
    let group = ContainedProcessGroup::with_originator(&tool_name).expect("create group");

    let mut cmd = Command::new(&sleeper);
    cmd.stdout(std::process::Stdio::piped());
    let child = group.spawn(&mut cmd).expect("spawn");
    let child_pid = child.child.id();

    std::thread::sleep(Duration::from_millis(500));

    let results = find_processes_by_originator("NONEXISTENT_TOOL_XYZ");
    let found = results.iter().any(|r| r.pid == child_pid);
    assert!(
        !found,
        "should NOT find child PID {child_pid} with wrong tool"
    );

    drop(group);
    force_kill(child_pid);
}
