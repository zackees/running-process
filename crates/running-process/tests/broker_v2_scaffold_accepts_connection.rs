//! Integration test for the slice 3c v2 broker scaffold.
//!
//! Spawns `running-process-broker-v2`, waits for it to print "bound at
//! <path>", connects to that path, and verifies the binary exits 0
//! within a deadline. Proves end-to-end that the v2 binary actually
//! claims a kernel resource on the host platform.

#![cfg(feature = "client")]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::Stream;

const DEADLINE: Duration = Duration::from_secs(10);

/// `--no-bind` short-circuit: the binary should print its banner and
/// exit 0 without touching the kernel namespace. Useful as a
/// platform-neutral build smoke test.
#[test]
fn binary_exits_clean_with_no_bind_flag() {
    let path = env!("CARGO_BIN_EXE_running-process-broker-v2");
    let output = Command::new(path)
        .arg("--no-bind")
        .output()
        .expect("spawn binary");

    assert!(
        output.status.success(),
        "--no-bind exit: {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("running-process-broker-v2"),
        "expected version banner in stdout, got: {stdout}"
    );
}

/// Full bind/accept round-trip: spawn the binary, parse the bound path
/// from stdout, dial it, and assert the binary exits 0.
#[test]
fn binary_binds_pipe_accepts_connection_and_exits() {
    let path = env!("CARGO_BIN_EXE_running-process-broker-v2");
    let mut child = Command::new(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn binary");

    let stdout = child
        .stdout
        .take()
        .expect("piped stdout must exist after spawn");
    let mut reader = BufReader::new(stdout);

    // Read until we see "bound at <path>" or timeout. The binary prints
    // the banner first, then the "bound at" line once the listener is
    // up, then waits for an `accept` call.
    let start = Instant::now();
    let mut socket_path: Option<String> = None;
    let mut all_stdout = String::new();
    while start.elapsed() < DEADLINE {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                all_stdout.push_str(&line);
                if let Some(rest) = line.strip_prefix("running-process-broker-v2 bound at ") {
                    socket_path = Some(rest.trim_end().to_string());
                    break;
                }
            }
            Err(err) => {
                // best-effort cleanup
                let _ = child.kill();
                panic!("read stdout: {err}\ncaptured so far: {all_stdout}");
            }
        }
    }

    let socket_path = match socket_path {
        Some(p) => p,
        None => {
            let _ = child.kill();
            panic!(
                "did not observe 'bound at' line within {:?}; captured stdout:\n{all_stdout}",
                DEADLINE
            );
        }
    };

    // Dial the same pipe the binary just bound, which unblocks its
    // `accept` call. The peer connection itself is short-lived — we
    // drop the stream immediately and let the binary close on its
    // side after observing the connect.
    let name = wrap_socket_name(&socket_path).expect("wrap_socket_name");
    let _stream = Stream::connect(name).expect("connect to v2 broker pipe");

    // Drain any remaining stdout so the binary can flush cleanly.
    let mut tail = String::new();
    let _ = reader.read_to_string(&mut tail);
    all_stdout.push_str(&tail);

    // Wait for exit — bounded by the same deadline.
    let exit = wait_with_deadline(&mut child, DEADLINE).expect("binary exited within deadline");
    assert!(
        exit.success(),
        "v2 binary exit: {:?}\nstdout: {all_stdout}",
        exit
    );

    assert!(
        all_stdout.contains("peer connected"),
        "expected 'peer connected' in stdout, got:\n{all_stdout}"
    );
}

fn wrap_socket_name(socket_path: &str) -> Result<interprocess::local_socket::Name<'_>, String> {
    use interprocess::local_socket::prelude::*;
    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        let bare = socket_path
            .strip_prefix(r"\\.\pipe\")
            .unwrap_or(socket_path);
        bare.to_ns_name::<GenericNamespaced>()
            .map_err(|e| format!("to_ns_name: {e}"))
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        socket_path
            .to_fs_name::<GenericFilePath>()
            .map_err(|e| format!("to_fs_name: {e}"))
    }
}

fn wait_with_deadline(
    child: &mut std::process::Child,
    deadline: Duration,
) -> Result<std::process::ExitStatus, String> {
    let start = Instant::now();
    while start.elapsed() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(err) => return Err(format!("try_wait failed: {err}")),
        }
    }
    let _ = child.kill();
    Err(format!("binary did not exit within {deadline:?}"))
}

// `read_to_string` is on `Read`, not `BufRead`; import it explicitly so
// the drain at the end of the bind test compiles.
use std::io::Read as _;
