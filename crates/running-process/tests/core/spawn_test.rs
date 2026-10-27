//! Wrapper-mode spawn invariants (issue #113).
//!
//! 1. test_spawn_inherited_stdio_reaches_parent — Pipe → child writes, parent reads.
//! 2. test_spawn_orphans_still_blocked — second inheritable pipe NOT in stdio
//!    stays in parent.
//! 3. test_spawn_force_killed_parent_reaps_child — intermediate parent exits,
//!    grandchild dies.
//! 4. test_spawn_child_exit_bounded_drain — child writes & exits, parent reads
//!    after delay then EOFs.

use std::io::Read;
#[cfg(not(target_os = "macos"))]
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use running_process::{
    spawn, spawn_daemon_with_stdio, DaemonStdio, DaemonStdioSource, SpawnStdio, StdioSource,
};

fn testbin_path(name: &str) -> PathBuf {
    // Fixtures are built once, before the suite runs (see `ci/test.py`).
    //
    // This used to invoke `cargo build -p testbins` on every call. That takes
    // cargo's build-directory lock, and nextest runs each test in its own
    // process, so a full-suite run had dozens of cargo invocations contending
    // for one lock. `Command::output` waits for EOF with no deadline, and
    // cargo's "Blocking waiting for file lock" note went to inherited stderr
    // the harness only shows on failure — so it presented as an unexplained
    // 30s+ hang. See running-process#747 for the symbolized stack.
    let exe = std::env::current_exe().expect("current exe");
    // .../target/<triple>/<profile>/deps/<test-binary>
    let profile_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    let path = profile_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "test fixture `{name}` is missing at {}.
         Build the fixtures first:  soldr cargo build -p testbins",
        path.display()
    );
    path
}

#[cfg(all(windows, not(target_os = "macos")))]
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
        exit_code == 259
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn is_pid_alive(pid: u32) -> bool {
    unsafe {
        let mut status: i32 = 0;
        let ret = libc::waitpid(pid as i32, &mut status, libc::WNOHANG);
        if ret == pid as i32 {
            return false;
        }
    }
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(target_os = "macos"))]
fn wait_until_dead(pid: u32, timeout: Duration) -> bool {
    let start = Instant::now();
    while is_pid_alive(pid) {
        if start.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

#[cfg(all(windows, not(target_os = "macos")))]
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

#[cfg(all(unix, not(target_os = "macos")))]
fn force_kill(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

#[cfg(not(target_os = "macos"))]
fn parse_pid_line(line: &str, prefix: &str) -> Option<u32> {
    line.strip_prefix(prefix)
        .and_then(|s| s.trim().parse::<u32>().ok())
}

// ── Pipe helper (mirrors containment_test.rs but local to keep tests self-contained) ──

#[cfg(windows)]
mod pipe_helpers {
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
        InheritablePipe {
            read_end,
            write_end,
        }
    }

    pub fn close(h: HANDLE) {
        if !h.is_null() {
            unsafe {
                CloseHandle(h);
            }
        }
    }

    pub fn read_one_byte_with_timeout(
        read_end: HANDLE,
        timeout: Duration,
    ) -> Result<usize, &'static str> {
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
mod pipe_helpers {
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

// ── Tests ───────────────────────────────────────────────────────────────────

fn shell_echo_cmd(text: &str) -> Command {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(format!("echo {text}"));
        cmd
    }
    #[cfg(unix)]
    {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!("echo {text}"));
        cmd
    }
}

/// 1. Pipe stdout reaches the parent.
#[test]
fn test_spawn_inherited_stdio_reaches_parent() {
    let mut cmd = shell_echo_cmd("hello");
    let stdio = SpawnStdio {
        stdin: StdioSource::Null,
        stdout: StdioSource::Pipe,
        stderr: StdioSource::Null,
        drain_timeout: Some(Duration::from_secs(2)),
        show_console: false,
    };
    let mut child = spawn(&mut cmd, stdio).expect("spawn");
    let mut stdout = child.stdout.take().expect("stdout");

    let mut buf = Vec::new();
    stdout.read_to_end(&mut buf).expect("read");
    let s = String::from_utf8_lossy(&buf);
    assert!(
        s.contains("hello"),
        "expected stdout to contain 'hello', got {s:?}"
    );

    let _ = child.wait();
}

/// 2. An inheritable pipe NOT passed to `stdio` must not appear in the
///    child — closing the parent's copy of the write-end yields EOF on
///    the read-end promptly.
#[test]
fn test_spawn_orphans_still_blocked() {
    use pipe_helpers::*;

    let sleeper = testbin_path("testbin-sleeper");

    // The orphan pipe: created inheritable, never handed to the child.
    let pipe = create_inheritable_pipe();

    // Spawn a long-lived child via the new sanitized path. Default stdio
    // means stdout/stderr inherit (no pipe slot), stdin = NUL.
    let mut cmd = Command::new(&sleeper);
    let mut child = spawn(&mut cmd, SpawnStdio::default()).expect("spawn");
    let child_pid = child.id();

    // Close our copy of the write-end. If the child inherited a dup,
    // the kernel ref-count stays > 0 and read blocks.
    close(pipe.write_end);

    let start = Instant::now();
    let result = read_one_byte_with_timeout(pipe.read_end, Duration::from_secs(2));
    let elapsed = start.elapsed();
    close(pipe.read_end);

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        matches!(result, Ok(0)),
        "expected EOF on orphan read-end, got {result:?} after {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "EOF should be prompt; took {elapsed:?}"
    );
    let _ = child_pid;
}

/// 3. When an intermediate parent (spawned via `spawn`) dies, the
///    grandchild it spawned (also via `spawn`) dies too.
///
/// Skipped on macOS: `PR_SET_PDEATHSIG` is unavailable there, so the
/// grandchild only dies via the wrapper's explicit `Drop` (which our
/// testbin deliberately skips via `mem::forget` to test the OS-level
/// containment). The Job Object (Windows) and Linux PDEATHSIG paths give
/// kernel-driven reaping; macOS would need a polling getppid() loop in
/// the child, which is out of scope for #113.
#[cfg(not(target_os = "macos"))]
#[test]
fn test_spawn_force_killed_parent_reaps_child() {
    let dies_after = testbin_path("testbin-dies-after-spawn");
    let sleeper = testbin_path("testbin-sleeper");

    let mut cmd = Command::new(&dies_after);
    cmd.env("RUNNING_PROCESS_SPAWN_TARGET", &sleeper);
    let stdio = SpawnStdio {
        stdin: StdioSource::Null,
        stdout: StdioSource::Pipe,
        stderr: StdioSource::Parent,
        drain_timeout: Some(Duration::from_secs(2)),
        show_console: false,
    };
    let mut child = spawn(&mut cmd, stdio).expect("spawn intermediate");
    let intermediate_pid = child.id();

    let stdout = child.stdout.take().expect("stdout");
    let reader = BufReader::new(stdout);

    let mut grandchild_pid: Option<u32> = None;
    let start = Instant::now();
    for line in reader.lines() {
        if start.elapsed() > Duration::from_secs(10) {
            panic!("timed out waiting for READY");
        }
        let line = line.expect("read");
        if let Some(pid) = parse_pid_line(&line, "GRANDCHILD_PID=") {
            grandchild_pid = Some(pid);
        } else if line.trim() == "READY" {
            break;
        }
    }
    let grandchild_pid = grandchild_pid.expect("should get grandchild PID");

    // Wait for intermediate to exit (it does so right after READY).
    let _ = child.wait();
    assert!(
        wait_until_dead(intermediate_pid, Duration::from_secs(5)),
        "intermediate parent should exit promptly"
    );

    // Grandchild was spawned via `spawn` inside the intermediate — its
    // Job Object handle (Windows) was leaked via mem::forget, so when
    // the intermediate process exits, the kernel closes its handles and
    // KILL_ON_JOB_CLOSE fires. On Unix, PR_SET_PDEATHSIG fires.
    let dead = wait_until_dead(grandchild_pid, Duration::from_secs(5));

    if !dead {
        // Cleanup before panic.
        force_kill(grandchild_pid);
    }
    assert!(
        dead,
        "grandchild (PID {grandchild_pid}) should die when intermediate parent exits"
    );
}

/// 4. Child writes some bytes then exits. Parent doesn't read for a while;
///    after the drain timeout the queued bytes are still readable, then
///    the next read returns EOF.
#[test]
fn test_spawn_child_exit_bounded_drain() {
    let mut cmd = shell_echo_cmd("done");
    let stdio = SpawnStdio {
        stdin: StdioSource::Null,
        stdout: StdioSource::Pipe,
        stderr: StdioSource::Null,
        drain_timeout: Some(Duration::from_secs(2)),
        show_console: false,
    };
    let mut child = spawn(&mut cmd, stdio).expect("spawn");
    let mut stdout = child.stdout.take().expect("stdout");

    // Wait for the child to exit + drain window + slack.
    let _ = child.wait();
    std::thread::sleep(Duration::from_secs(3));

    let mut buf = Vec::new();
    let n = stdout.read_to_end(&mut buf).expect("read after exit");
    let s = String::from_utf8_lossy(&buf);
    assert!(n > 0, "expected queued bytes to be readable post-exit");
    assert!(s.contains("done"), "expected 'done' in stdout, got {s:?}");

    // Subsequent read should immediately return EOF.
    let mut more = [0u8; 16];
    let n2 = stdout.read(&mut more).expect("eof read");
    assert_eq!(n2, 0, "expected EOF on next read, got {n2} bytes");
}

/// Detached daemons may write to caller-owned files without inheriting parent
/// streams or retaining the caller's handles.
#[test]
fn test_spawn_daemon_file_stdio_is_preserved() {
    let log = tempfile::NamedTempFile::new().expect("temp log");
    let stdout = log.reopen().expect("open stdout log");
    let stderr = log.reopen().expect("open stderr log");
    let mut command = shell_echo_cmd("daemon-file-stdio");
    let stdio = DaemonStdio {
        stdout: DaemonStdioSource::File(&stdout),
        stderr: DaemonStdioSource::File(&stderr),
    };

    let mut child = spawn_daemon_with_stdio(&mut command, stdio).expect("spawn daemon");
    drop(stdout);
    drop(stderr);
    assert_eq!(child.wait().expect("wait for daemon"), 0);

    let body = std::fs::read_to_string(log.path()).expect("read daemon log");
    assert!(
        body.contains("daemon-file-stdio"),
        "daemon output did not reach its file: {body:?}"
    );
}

#[test]
fn spawn_argv_streams_exact_binary_into_owned_files() {
    use std::ffi::OsString;
    use std::fs::OpenOptions;

    let dir = tempfile::tempdir().expect("temporary directory");
    let output_path = dir.path().join("stdout.bin");
    let error_path = dir.path().join("stderr.bin");
    let output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&output_path)
        .expect("open output");
    let errors = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&error_path)
        .expect("open errors");
    let argv: Vec<OsString> = [
        testbin_path("testbin-stdio-scripted").into_os_string(),
        "echo".into(),
        "outrep:000d0aff:100000".into(),
        "outhex:fe".into(),
        "errhex:fa0d0a".into(),
        "exit:17".into(),
    ]
    .into();
    let mut child = running_process::spawn_argv_with_stdio(
        &argv,
        None,
        None,
        SpawnStdio {
            stdin: StdioSource::Pipe,
            stdout: StdioSource::File(&output),
            stderr: StdioSource::File(&errors),
            ..SpawnStdio::default()
        },
    )
    .expect("spawn child with owned files");
    drop(output);
    drop(errors);
    drop(child.stdin.take().expect("stdin handshake"));
    assert_eq!(child.wait().expect("wait for child"), 17);
    let mut expected = [0, 13, 10, 255].repeat(100_000);
    expected.push(254);
    assert_eq!(std::fs::read(output_path).expect("read output"), expected);
    assert_eq!(
        std::fs::read(error_path).expect("read stderr"),
        [250, 13, 10]
    );
}

#[test]
fn spawn_argv_rejects_missing_executable() {
    let error = match running_process::spawn_argv_with_stdio(&[], None, None, SpawnStdio::default())
    {
        Ok(_) => panic!("empty argv must not spawn a process"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn spawn_argv_keeps_the_opened_file_when_the_path_is_replaced() {
    use std::fs::OpenOptions;

    let dir = tempfile::tempdir().expect("temporary directory");
    let original = dir.path().join("original");
    let moved = dir.path().join("moved");
    let output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&original)
        .expect("open output");
    std::fs::rename(&original, &moved).expect("move opened file");
    std::fs::write(&original, b"replacement").expect("create replacement");
    let argv = [
        testbin_path("testbin-stdio-scripted").into_os_string(),
        "outhex:00ff0d0a".into(),
    ];
    let mut child = running_process::spawn_argv_with_stdio(
        &argv,
        None,
        None,
        SpawnStdio {
            stdout: StdioSource::File(&output),
            ..SpawnStdio::default()
        },
    )
    .expect("spawn child");
    assert_eq!(child.wait().expect("wait"), 0);
    assert_eq!(
        std::fs::read(moved).expect("read opened file"),
        [0, 255, 13, 10]
    );
    assert_eq!(
        std::fs::read(original).expect("read replacement"),
        b"replacement"
    );
}

#[test]
fn spawn_argv_forwards_cwd_environment_and_literal_arguments() {
    use std::ffi::OsString;
    use std::fs::OpenOptions;

    let dir = tempfile::tempdir().expect("temporary directory");
    let output_path = dir.path().join("output");
    let output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&output_path)
        .expect("open output");
    let argv = [
        testbin_path("testbin-stdio-scripted").into_os_string(),
        "cwd".into(),
        "out:|".into(),
        "env:RP_FILE_SINK_TEST".into(),
        "out:|literal $name & value".into(),
    ];
    let env = [(
        OsString::from("RP_FILE_SINK_TEST"),
        OsString::from("hello value"),
    )];
    let mut child = running_process::spawn_argv_with_stdio(
        &argv,
        Some(dir.path()),
        Some(&env),
        SpawnStdio {
            stdout: StdioSource::File(&output),
            ..SpawnStdio::default()
        },
    )
    .expect("spawn child");
    assert_eq!(child.wait().expect("wait"), 0);
    let captured = std::fs::read_to_string(output_path).expect("read output");
    let (cwd, suffix) = captured.split_once('|').expect("cwd separator");
    assert_eq!(
        std::fs::canonicalize(cwd).expect("child cwd"),
        dir.path().canonicalize().unwrap()
    );
    assert_eq!(suffix, "hello value|literal $name & value");
}

#[test]
fn spawn_argv_child_can_be_cancelled_without_hiding_its_failure() {
    use std::fs::OpenOptions;

    let dir = tempfile::tempdir().expect("temporary directory");
    let output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(dir.path().join("output"))
        .expect("open output");
    let argv = [
        testbin_path("testbin-stdio-scripted").into_os_string(),
        "sleep-ms:30000".into(),
    ];
    let mut child = running_process::spawn_argv_with_stdio(
        &argv,
        None,
        None,
        SpawnStdio {
            stdout: StdioSource::File(&output),
            ..SpawnStdio::default()
        },
    )
    .expect("spawn child");
    child.kill().expect("cancel child");
    assert_ne!(child.wait().expect("wait for cancelled child"), 0);
}

#[cfg(windows)]
#[test]
fn spawn_argv_preserves_non_scalar_windows_arguments() {
    use std::ffi::OsString;
    use std::fs::OpenOptions;
    use std::os::windows::ffi::OsStringExt;

    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("output");
    let output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .unwrap();
    let units = [0xd800, b' ' as u16, b'"' as u16, b'\\' as u16];
    let argv = [
        testbin_path("testbin-stdio-scripted").into_os_string(),
        "report-wide".into(),
        OsString::from_wide(&units),
    ];
    let mut child = running_process::spawn_argv_with_stdio(
        &argv,
        None,
        None,
        SpawnStdio {
            stdout: StdioSource::File(&output),
            ..SpawnStdio::default()
        },
    )
    .expect("spawn child");
    drop(output);
    assert_eq!(child.wait().expect("wait"), 0);
    let expected: Vec<u8> = units.into_iter().flat_map(u16::to_le_bytes).collect();
    assert_eq!(std::fs::read(path).unwrap(), expected);
}

#[test]
fn spawn_argv_rejects_nul_and_invalid_environment_before_spawn() {
    use std::ffi::OsString;

    let program = testbin_path("testbin-stdio-scripted").into_os_string();
    let invalid_args = [program.clone(), OsString::from("before\0after")];
    assert!(running_process::spawn_argv_with_stdio(
        &invalid_args,
        None,
        None,
        SpawnStdio::default()
    )
    .is_err());
    let invalid_env = [(OsString::from("BAD=KEY"), OsString::from("value"))];
    assert!(running_process::spawn_argv_with_stdio(
        &[program],
        None,
        Some(&invalid_env),
        SpawnStdio::default()
    )
    .is_err());
}

#[cfg(unix)]
#[test]
fn spawn_argv_retains_leader_identity_until_cleanup() {
    let argv = [
        testbin_path("testbin-stdio-scripted").into_os_string(),
        "exit:0".into(),
    ];
    let mut child =
        running_process::spawn_argv_with_stdio(&argv, None, None, SpawnStdio::default())
            .expect("spawn child");
    let pid = child.id();
    assert_eq!(child.wait().expect("wait"), 0);
    // Exceed the existing drain watcher's 50 ms sampling interval.
    std::thread::sleep(Duration::from_millis(250));
    // SAFETY: `info` is a valid output buffer and `pid` is our still-owned child.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: waitid writes to `info`; WNOWAIT leaves the child identity reserved.
    let status = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    assert_eq!(status, 0, "child identity was reaped before cleanup");
    drop(child);
}

#[cfg(not(target_os = "macos"))]
#[test]
fn spawn_argv_tree_cancellation_stops_descendants() {
    use std::fs::OpenOptions;

    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("output");
    let output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .expect("open output");
    let argv = [
        testbin_path("testbin-spawner").into_os_string(),
        "1".into(),
        testbin_path("testbin-sleeper").into_os_string(),
    ];
    let mut child = running_process::spawn_argv_with_stdio(
        &argv,
        None,
        None,
        SpawnStdio {
            stdout: StdioSource::File(&output),
            ..SpawnStdio::default()
        },
    )
    .expect("spawn child");
    drop(output);
    let deadline = Instant::now() + Duration::from_secs(30);
    let grandchild = loop {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if text.contains("READY") {
            let pid = text
                .lines()
                .find_map(|line| line.strip_prefix("CHILD_PID="));
            break pid.expect("child PID").parse::<u32>().expect("numeric PID");
        }
        assert!(Instant::now() < deadline, "spawner did not become ready");
        std::thread::sleep(Duration::from_millis(25));
    };
    child.kill_tree().expect("terminate tree");
    assert_ne!(child.wait().expect("wait for leader"), 0);
    let dead = wait_until_dead(grandchild, Duration::from_secs(10));
    if !dead {
        force_kill(grandchild);
    }
    assert!(dead, "descendant survived tree cancellation");
}

#[cfg(windows)]
#[test]
fn spawn_argv_rejects_ambiguous_windows_lookup_and_shell_scripts() {
    use std::ffi::OsString;

    let env = [(OsString::from("PATH"), OsString::from("C:\\other"))];
    assert!(running_process::spawn_argv_with_stdio(
        &[OsString::from("gh")],
        None,
        Some(&env),
        SpawnStdio::default()
    )
    .is_err());
    let dir = tempfile::tempdir().expect("working directory");
    assert!(running_process::spawn_argv_with_stdio(
        &[OsString::from("gh.exe")],
        Some(dir.path()),
        None,
        SpawnStdio::default()
    )
    .is_err());
    let argv = [
        OsString::from("cmd.exe"),
        OsString::from("/C"),
        OsString::from("echo safe"),
    ];
    assert!(
        running_process::spawn_argv_with_stdio(&argv, None, None, SpawnStdio::default()).is_err()
    );
}

#[cfg(unix)]
#[test]
fn spawn_argv_pipe_eof_waits_for_the_last_descendant_writer() {
    use std::io::Read;
    use std::sync::mpsc;

    let argv = [
        testbin_path("testbin-session-pipe-holder").into_os_string(),
        testbin_path("testbin-sleeper").into_os_string(),
    ];
    let mut child = running_process::spawn_argv_with_stdio(
        &argv,
        None,
        None,
        SpawnStdio {
            stdout: StdioSource::Pipe,
            drain_timeout: Some(Duration::from_millis(100)),
            ..SpawnStdio::default()
        },
    )
    .expect("spawn pipe holder");
    let mut stdout = child.stdout.take().expect("stdout pipe");
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.read_to_end(&mut bytes).map(|_| bytes);
        let _ = send.send(result);
    });
    assert_eq!(child.wait().expect("wait for leader"), 0);
    assert!(matches!(
        receive.recv_timeout(Duration::from_millis(250)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    child.kill_tree().expect("stop remaining writer");
    drop(child);
    let bytes = receive
        .recv_timeout(Duration::from_secs(10))
        .expect("pipe EOF")
        .expect("read");
    assert!(String::from_utf8_lossy(&bytes).contains("GRANDCHILD_PID="));
}

#[cfg(windows)]
#[test]
fn spawn_argv_does_not_inherit_unrelated_handles() {
    use pipe_helpers::*;
    use std::fs::OpenOptions;

    let pipe = create_inheritable_pipe();
    let dir = tempfile::tempdir().expect("temporary directory");
    let output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(dir.path().join("output"))
        .expect("open output");
    let argv = [testbin_path("testbin-sleeper").into_os_string()];
    let mut child = running_process::spawn_argv_with_stdio(
        &argv,
        None,
        None,
        SpawnStdio {
            stdout: StdioSource::File(&output),
            ..SpawnStdio::default()
        },
    )
    .expect("spawn child");
    close(pipe.write_end);
    let result = read_one_byte_with_timeout(pipe.read_end, Duration::from_secs(2));
    close(pipe.read_end);
    child.kill_tree().expect("terminate child");
    let _ = child.wait();
    assert!(
        matches!(result, Ok(0)),
        "unrelated handle leaked: {result:?}"
    );
}

#[cfg(windows)]
fn windows_file_dacl(file: &std::fs::File) -> Vec<u16> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
        SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;

    let mut descriptor = std::ptr::null_mut();
    // SAFETY: the file handle is live and descriptor is a valid out pointer.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle().cast(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(status, 0, "GetSecurityInfo failed");
    let mut rendered = std::ptr::null_mut();
    let mut length = 0;
    // SAFETY: descriptor is live and both output pointers are valid.
    let converted = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut rendered,
            &mut length,
        )
    };
    assert_ne!(converted, 0, "DACL conversion failed");
    // SAFETY: the conversion returned `length` UTF-16 units at rendered.
    let result = unsafe { std::slice::from_raw_parts(rendered, length as usize) }.to_vec();
    // SAFETY: both allocations are owned Windows LocalAlloc outputs.
    unsafe {
        LocalFree(rendered.cast());
        LocalFree(descriptor.cast());
    }
    result
}

#[cfg(windows)]
#[test]
fn spawn_argv_preserves_the_caller_file_dacl() {
    use std::fs::OpenOptions;

    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("output");
    let output = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .expect("open output");
    let before = windows_file_dacl(&output);
    let argv = [
        testbin_path("testbin-stdio-scripted").into_os_string(),
        "outhex:00ff".into(),
    ];
    let mut child = running_process::spawn_argv_with_stdio(
        &argv,
        None,
        None,
        SpawnStdio {
            stdout: StdioSource::File(&output),
            ..SpawnStdio::default()
        },
    )
    .expect("spawn child");
    assert_eq!(child.wait().expect("wait"), 0);
    assert_eq!(before, windows_file_dacl(&output));
    drop(output);
    assert_eq!(std::fs::read(path).expect("read output"), [0, 255]);
}

#[test]
fn spawn_argv_timeout_preserves_output_and_nonzero_status() {
    use std::fs::OpenOptions;

    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("output");
    let output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .expect("open output");
    let argv = [
        testbin_path("testbin-stdio-scripted").into_os_string(),
        "out:started".into(),
        "sleep-ms:30000".into(),
    ];
    let mut child = running_process::spawn_argv_with_stdio(
        &argv,
        None,
        None,
        SpawnStdio {
            stdout: StdioSource::File(&output),
            ..SpawnStdio::default()
        },
    )
    .expect("spawn child");
    drop(output);
    let setup_deadline = Instant::now() + Duration::from_secs(30);
    while std::fs::read(&path).unwrap_or_default() != b"started" {
        assert!(Instant::now() < setup_deadline, "child did not start");
        std::thread::sleep(Duration::from_millis(25));
    }
    let deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < deadline {
        assert!(child.try_wait().expect("check child").is_none());
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill_tree().expect("stop timed-out child");
    assert_ne!(child.wait().expect("wait for child"), 0);
    drop(child);
    assert_eq!(std::fs::read(path).expect("read output"), b"started");
}

#[cfg(unix)]
#[test]
fn identity_retention_rejects_an_active_reaping_watcher() {
    let mut command = Command::new(testbin_path("testbin-stdio-scripted"));
    command.arg("sleep-ms:30000");
    let mut child = spawn(&mut command, SpawnStdio::default()).expect("spawn child");
    let error = child
        .retain_exit_identity()
        .expect_err("watcher must be disabled");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    child.kill_tree().expect("stop child");
    let _ = child.wait();
}

#[cfg(unix)]
#[test]
fn identity_retention_rejects_a_reaped_child() {
    let mut command = Command::new(testbin_path("testbin-stdio-scripted"));
    let stdio = SpawnStdio {
        drain_timeout: None,
        ..SpawnStdio::default()
    };
    let mut child = spawn(&mut command, stdio).expect("spawn child");
    child.wait().expect("reap child");
    child
        .retain_exit_identity()
        .expect_err("reaped identity cannot be retained");
}

#[cfg(unix)]
#[test]
fn spawn_argv_reports_negative_signal_in_wait_and_try_wait() {
    let argv = [
        testbin_path("testbin-stdio-scripted").into_os_string(),
        "sleep-ms:30000".into(),
    ];
    let mut child =
        running_process::spawn_argv_with_stdio(&argv, None, None, SpawnStdio::default())
            .expect("spawn");
    child.kill_tree().expect("kill tree");
    assert_eq!(child.wait().expect("wait"), -9);
    assert_eq!(child.try_wait().expect("try wait"), Some(-9));
}
