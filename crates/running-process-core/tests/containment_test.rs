//! Integration tests for `ContainedProcessGroup`.
//!
//! These tests spawn real processes and verify that containment works:
//! - Contained children die when the group is dropped.
//! - Grandchildren (spawned by children) also die.
//! - Detached children survive the group being dropped.
//!
//! Run with `--test-threads=1` on Windows to avoid Job Object conflicts.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use running_process_core::ContainedProcessGroup;

/// Build (if needed) and locate a test binary from the workspace.
///
/// `cargo test` does not guarantee that binary targets from sibling workspace
/// members are built before integration tests run.  We shell out to
/// `cargo build -p <name>` to ensure the binary exists, then return the path
/// from the `--message-format=json` output so it works on every platform and
/// target-directory layout.
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

    // Parse the JSON lines to find the compiler artifact for the binary.
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        // Quick pre-filter before parsing JSON.
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

/// Check whether a process with the given PID is still alive.
#[cfg(windows)]
fn is_pid_alive(pid: u32) -> bool {
    unsafe {
        let handle = winapi::um::processthreadsapi::OpenProcess(
            winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        );
        if handle.is_null() {
            return false;
        }
        let mut exit_code: u32 = 0;
        let ok =
            winapi::um::processthreadsapi::GetExitCodeProcess(handle, &mut exit_code as *mut u32);
        winapi::um::handleapi::CloseHandle(handle);
        if ok == 0 {
            return false;
        }
        // STILL_ACTIVE = 259
        exit_code == 259
    }
}

#[cfg(unix)]
fn is_pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Wait until a PID is dead, or panic after timeout.
fn wait_until_dead(pid: u32, timeout: Duration) {
    let start = Instant::now();
    while is_pid_alive(pid) {
        if start.elapsed() > timeout {
            panic!("PID {pid} still alive after {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Kill a process by PID (best-effort cleanup).
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

/// Parse a line like "PID=1234" and return 1234.
fn parse_pid_line(line: &str, prefix: &str) -> Option<u32> {
    line.strip_prefix(prefix)
        .and_then(|s| s.trim().parse::<u32>().ok())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn test_contained_group_kills_on_drop() {
    let sleeper = testbin_path("testbin-sleeper");
    let group = ContainedProcessGroup::new().expect("create group");

    // Spawn two sleepers inside the group.
    let mut cmd1 = Command::new(&sleeper);
    cmd1.stdout(std::process::Stdio::piped());
    let child1 = group.spawn(&mut cmd1).expect("spawn 1");
    let pid1 = child1.child.id();

    let mut cmd2 = Command::new(&sleeper);
    cmd2.stdout(std::process::Stdio::piped());
    let child2 = group.spawn(&mut cmd2).expect("spawn 2");
    let pid2 = child2.child.id();

    // Give the children a moment to start.
    std::thread::sleep(Duration::from_millis(200));

    // Both should be alive.
    assert!(
        is_pid_alive(pid1),
        "child 1 (PID {pid1}) should be alive after spawn"
    );
    assert!(
        is_pid_alive(pid2),
        "child 2 (PID {pid2}) should be alive after spawn"
    );

    // Drop the group — this should kill both children.
    drop(group);

    // Give the OS a moment to reap.
    let timeout = Duration::from_secs(10);
    wait_until_dead(pid1, timeout);
    wait_until_dead(pid2, timeout);
}

#[test]
fn test_contained_group_kills_grandchildren() {
    let sleeper = testbin_path("testbin-sleeper");
    let spawner = testbin_path("testbin-spawner");
    let group = ContainedProcessGroup::new().expect("create group");

    // Spawn the spawner, which in turn spawns 2 sleeper grandchildren.
    let mut cmd = Command::new(&spawner);
    cmd.arg("2").arg(&sleeper);
    cmd.stdout(std::process::Stdio::piped());
    let mut child = group.spawn(&mut cmd).expect("spawn spawner");

    // Read the spawner's stdout to learn all PIDs.
    let stdout = child.child.stdout.take().expect("stdout");
    let reader = BufReader::new(stdout);

    let mut spawner_pid: Option<u32> = None;
    let mut grandchild_pids: Vec<u32> = Vec::new();

    let start = Instant::now();
    for line in reader.lines() {
        if start.elapsed() > Duration::from_secs(10) {
            panic!("timed out reading spawner output");
        }
        let line = line.expect("read line");
        if let Some(pid) = parse_pid_line(&line, "SPAWNER_PID=") {
            spawner_pid = Some(pid);
        } else if let Some(pid) = parse_pid_line(&line, "CHILD_PID=") {
            grandchild_pids.push(pid);
        } else if line.trim() == "READY" {
            break;
        }
    }

    let spawner_pid = spawner_pid.expect("spawner should print its PID");
    assert_eq!(
        grandchild_pids.len(),
        2,
        "spawner should have spawned 2 children"
    );

    // Verify everyone is alive.
    assert!(is_pid_alive(spawner_pid), "spawner should be alive");
    for &pid in &grandchild_pids {
        assert!(is_pid_alive(pid), "grandchild {pid} should be alive");
    }

    // Drop the group.
    drop(group);

    // All should die (on Windows, the Job Object kills the entire tree).
    let timeout = Duration::from_secs(10);
    wait_until_dead(spawner_pid, timeout);
    for &pid in &grandchild_pids {
        wait_until_dead(pid, timeout);
    }
}

#[test]
fn test_detached_survives_group_drop() {
    let sleeper = testbin_path("testbin-sleeper");
    let group = ContainedProcessGroup::new().expect("create group");

    // Spawn a contained child.
    let mut cmd_contained = Command::new(&sleeper);
    cmd_contained.stdout(std::process::Stdio::piped());
    let contained = group.spawn(&mut cmd_contained).expect("spawn contained");
    let contained_pid = contained.child.id();

    // Spawn a detached child.
    let mut cmd_detached = Command::new(&sleeper);
    cmd_detached.stdout(std::process::Stdio::piped());
    let detached = group
        .spawn_detached(&mut cmd_detached)
        .expect("spawn detached");
    let detached_pid = detached.child.id();

    // Give them a moment to start.
    std::thread::sleep(Duration::from_millis(200));

    // Both should be alive.
    assert!(
        is_pid_alive(contained_pid),
        "contained child should be alive"
    );
    assert!(is_pid_alive(detached_pid), "detached child should be alive");

    // Drop the group.
    drop(group);

    // Contained child should die.
    wait_until_dead(contained_pid, Duration::from_secs(10));

    // Detached child should still be alive.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        is_pid_alive(detached_pid),
        "detached child (PID {detached_pid}) should survive group drop"
    );

    // Clean up.
    force_kill(detached_pid);
    wait_until_dead(detached_pid, Duration::from_secs(5));
}
