//! Integration tests for `ContainedProcessGroup`.
//!
//! These tests spawn real processes and verify that containment works:
//! - Contained children die when the group is dropped.
//! - Grandchildren (spawned by children) also die.
//! - Detached children survive the group being dropped.
//! - Sanitized spawns do not duplicate orphaned inheritable handles
//!   from the parent into the child (issue #110).
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
    // Try to reap a zombie first.  After SIGKILL a child stays in the
    // process table as a zombie until waitpid() is called.  kill(pid, 0)
    // returns 0 for zombies, which would make us think the process is
    // alive when it's actually dead.  WNOHANG ensures we never block.
    // If the PID is not our child, waitpid returns -1/ECHILD — harmless.
    unsafe {
        let mut status: i32 = 0;
        let ret = libc::waitpid(pid as i32, &mut status, libc::WNOHANG);
        if ret == pid as i32 {
            return false; // was a zombie, now reaped — definitely dead
        }
    }
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

// ── Sanitized-spawn handle-leak tests (issue #110) ──────────────────────────
//
// Goal: prove that `spawn_sanitized` does NOT duplicate orphaned inheritable
// handles from the parent's table into the child. If the parent has an
// inheritable pipe write-end sitting around, and the parent later closes its
// own copy, the pipe reader must see EOF promptly — meaning no copy survived
// in the child.

#[cfg(windows)]
mod sanitized_pipe_helpers {
    use std::os::windows::io::RawHandle;
    use std::time::Duration;

    use winapi::shared::minwindef::{BOOL, DWORD, FALSE, TRUE};
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::minwinbase::SECURITY_ATTRIBUTES;
    use winapi::um::namedpipeapi::CreatePipe;
    use winapi::um::winnt::HANDLE;

    pub struct InheritablePipe {
        pub read_end: HANDLE,
        pub write_end: HANDLE,
    }

    /// Create an anonymous pipe whose write-end is marked inheritable.
    pub fn create_inheritable_pipe() -> InheritablePipe {
        let mut sa: SECURITY_ATTRIBUTES = unsafe { std::mem::zeroed() };
        sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as DWORD;
        sa.bInheritHandle = TRUE as BOOL;
        sa.lpSecurityDescriptor = std::ptr::null_mut();

        let mut read_end: HANDLE = std::ptr::null_mut();
        let mut write_end: HANDLE = std::ptr::null_mut();
        let ok = unsafe {
            CreatePipe(
                &mut read_end as *mut HANDLE,
                &mut write_end as *mut HANDLE,
                &mut sa as *mut SECURITY_ATTRIBUTES,
                0,
            )
        };
        assert!(ok != FALSE, "CreatePipe failed");
        InheritablePipe { read_end, write_end }
    }

    /// Close a single handle.
    pub fn close(h: HANDLE) {
        if !h.is_null() {
            unsafe {
                CloseHandle(h);
            }
        }
    }

    /// Try to read 1 byte from `read_end` with the given timeout.
    /// Returns `Ok(0)` on EOF (write end closed by every process holding it),
    /// `Err(...)` on timeout.
    pub fn read_one_byte_with_timeout(
        read_end: HANDLE,
        timeout: Duration,
    ) -> Result<usize, &'static str> {
        // Use a thread + channel: ReadFile on a blocking handle has no native
        // timeout, so we read on a worker thread and wait for it.
        let h = read_end as RawHandle as usize;
        let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<usize>>();
        std::thread::spawn(move || {
            let h = h as RawHandle;
            let mut buf = [0u8; 1];
            let mut got: DWORD = 0;
            let ok = unsafe {
                winapi::um::fileapi::ReadFile(
                    h as HANDLE,
                    buf.as_mut_ptr().cast(),
                    1,
                    &mut got as *mut DWORD,
                    std::ptr::null_mut(),
                )
            };
            if ok == FALSE {
                let err = std::io::Error::last_os_error();
                // ERROR_BROKEN_PIPE means the other end closed → EOF.
                if err.raw_os_error() == Some(109) {
                    let _ = tx.send(Ok(0));
                } else {
                    let _ = tx.send(Err(err));
                }
            } else {
                let _ = tx.send(Ok(got as usize));
            }
        });

        match rx.recv_timeout(timeout) {
            Ok(Ok(n)) => Ok(n),
            Ok(Err(_)) => Err("read failed"),
            Err(_) => Err("timed out"),
        }
    }
}

#[cfg(unix)]
mod sanitized_pipe_helpers {
    use std::os::fd::RawFd;
    use std::time::Duration;

    pub struct InheritablePipe {
        pub read_end: RawFd,
        pub write_end: RawFd,
    }

    pub fn create_inheritable_pipe() -> InheritablePipe {
        let mut fds = [0 as libc::c_int; 2];
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert!(rc == 0, "pipe() failed");
        // Default Unix pipe() leaves CLOEXEC unset, so both ends are
        // inheritable across exec — matching the "legacy daemon" pattern.
        InheritablePipe {
            read_end: fds[0],
            write_end: fds[1],
        }
    }

    pub fn close(fd: RawFd) {
        unsafe {
            libc::close(fd);
        }
    }

    pub fn read_one_byte_with_timeout(
        read_end: RawFd,
        timeout: Duration,
    ) -> Result<usize, &'static str> {
        let mut pfd = libc::pollfd {
            fd: read_end,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
        let n = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, ms) };
        if n == 0 {
            return Err("timed out");
        }
        if n < 0 {
            return Err("poll failed");
        }
        let mut buf = [0u8; 1];
        let r = unsafe { libc::read(read_end, buf.as_mut_ptr().cast(), 1) };
        if r < 0 {
            return Err("read failed");
        }
        Ok(r as usize)
    }
}

/// `spawn_sanitized` must not duplicate orphaned inheritable handles
/// from the parent's handle table into the child.
///
/// Issue: zackees/running-process#110.
#[test]
fn test_sanitized_does_not_leak_inheritable_handles() {
    use sanitized_pipe_helpers::*;

    let sleeper = testbin_path("testbin-sleeper");
    let group = ContainedProcessGroup::new().expect("create group");

    // 1. Create a pipe whose write-end is inheritable.
    let pipe = create_inheritable_pipe();

    // 2. Spawn a long-lived child via the sanitized path. The child should
    //    NOT receive a duplicate of `pipe.write_end`.
    let mut cmd = Command::new(&sleeper);
    let mut child = group.spawn_sanitized(&mut cmd).expect("spawn sanitized");
    let child_pid = child.id();
    assert!(child_pid != 0, "child PID should be non-zero");

    // 3. Close the parent's only copy of the write-end. If the child also
    //    received a duplicate, the kernel reference count stays > 0 and the
    //    reader will block. If sanitized worked, refcount drops to zero and
    //    the reader sees EOF promptly.
    close(pipe.write_end);

    // 4. Reader must see EOF within ~1s.
    let start = Instant::now();
    let result = read_one_byte_with_timeout(pipe.read_end, Duration::from_secs(2));
    let elapsed = start.elapsed();
    close(pipe.read_end);

    // Clean up the child before asserting (so failure doesn't strand it).
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        matches!(result, Ok(0)),
        "expected EOF on read end, got {result:?} after {elapsed:?} — \
         child likely inherited an orphaned copy of the pipe write-end"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "EOF should be prompt; took {elapsed:?}"
    );
}

/// Control test: prove the leak detection actually works. The default
/// `spawn_detached` path uses Rust's `Command::spawn()` which calls
/// `CreateProcessW(bInheritHandles=TRUE)` on Windows and leaves
/// non-`O_CLOEXEC` fds open across `exec` on Unix. Either way, an
/// inheritable pipe write-end IS duplicated into the child — so closing
/// our copy must NOT produce EOF.
///
/// This test exists to make sure the sanitized-spawn test above is
/// actually proving something. If this test ever starts failing it means
/// the leak no longer reproduces and the sanitized test's value is
/// reduced.
#[test]
fn test_default_spawn_leaks_inheritable_handles_control() {
    use sanitized_pipe_helpers::*;

    let sleeper = testbin_path("testbin-sleeper");
    let group = ContainedProcessGroup::new().expect("create group");

    let pipe = create_inheritable_pipe();

    // Spawn through the leaky path.
    let mut cmd = Command::new(&sleeper);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let contained = group.spawn_detached(&mut cmd).expect("spawn detached");
    let child_pid = contained.child.id();

    close(pipe.write_end);

    // Reader should NOT see EOF — the child holds a duplicate.
    let result = read_one_byte_with_timeout(pipe.read_end, Duration::from_millis(500));
    close(pipe.read_end);

    // Kill the child to release the duplicate.
    force_kill(child_pid);
    wait_until_dead(child_pid, Duration::from_secs(5));

    assert!(
        result.is_err(),
        "default spawn should leak the pipe handle into the child, \
         keeping reader blocked; instead got {result:?}"
    );
}
