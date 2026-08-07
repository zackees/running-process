#![cfg(feature = "async-process")]

//! Cross-process proof that `AsyncProcess::kill_when_owner_dies` is an OS-level
//! relationship, not a best-effort `Drop` cleanup.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn testbin_path(name: &str) -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    let profile_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    let path = profile_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "test fixture is missing at {}",
        path.display()
    );
    path
}

fn parse_pid_line(line: &str, prefix: &str) -> Option<u32> {
    line.strip_prefix(prefix)?.trim().parse().ok()
}

fn wait_until_dead(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        #[cfg(unix)]
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
        #[cfg(windows)]
        let alive = {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            };
            const STILL_ACTIVE: u32 = 259;
            let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if handle.is_null() {
                false
            } else {
                let mut code = 0;
                let ok = unsafe { GetExitCodeProcess(handle, &mut code) } != 0;
                unsafe { CloseHandle(handle) };
                ok && code == STILL_ACTIVE
            }
        };
        if !alive {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[cfg(unix)]
fn force_kill(pid: u32) {
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) }, 0);
}

#[cfg(windows)]
fn force_kill(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    assert!(!handle.is_null(), "open owner process");
    assert_ne!(
        unsafe { TerminateProcess(handle, 1) },
        0,
        "terminate owner process"
    );
    unsafe { CloseHandle(handle) };
}

#[test]
fn force_killed_async_owner_reaps_child() {
    let owner = testbin_path("testbin-async-dies-after-spawn");
    let target = testbin_path("testbin-sleeper");
    let mut process = Command::new(owner)
        .arg(target)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn async owner fixture");
    let owner_pid = process.id();
    let stdout = process.stdout.take().expect("owner stdout");
    let mut child_pid = None;
    for line in BufReader::new(stdout).lines() {
        let line = line.expect("read owner output");
        if let Some(pid) = parse_pid_line(&line, "GRANDCHILD_PID=") {
            child_pid = Some(pid);
        }
        if line.trim() == "READY" {
            break;
        }
    }
    let child_pid = child_pid.expect("owner must report child pid");

    force_kill(owner_pid);
    let _ = process.wait();
    assert!(
        wait_until_dead(child_pid, Duration::from_secs(5)),
        "async child {child_pid} survived owner {owner_pid} termination"
    );
}
