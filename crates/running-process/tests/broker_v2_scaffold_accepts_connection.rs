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
///
/// PR #533 added ServiceDefinitionLoader integration; the scaffold
/// service name has to be registered or the Hello returns
/// ErrorServiceUnknown. This test installs a stub servicedef in a
/// tempdir + points the broker at it via `RUNNING_PROCESS_SERVICE_DEF_DIR`.
#[test]
fn binary_binds_pipe_accepts_connection_and_exits() {
    // Install a stub servicedef so the broker's loader accepts the
    // test Hello. Per-test tempdir keeps concurrent runs isolated.
    let svc_dir = tempfile::tempdir().expect("tempdir for servicedef");
    let stub_binary = if cfg!(windows) {
        svc_dir.path().join("scaffold-stub.exe")
    } else {
        svc_dir.path().join("scaffold-stub")
    };
    std::fs::write(&stub_binary, b"#!/bin/sh\necho stub\n").expect("write stub binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&stub_binary).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&stub_binary, perms).unwrap();
    }
    // Use a unique --program per test invocation so concurrent / repeated
    // runs don't collide on the global per-user pipe namespace
    // (Windows ERROR_ACCESS_DENIED when an old broker on the same pipe
    // hasn't released yet).
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Keep total program length under v2_program_pipe's 64-char max
    // (the pipe name is `rpb-v2-<program>-<sid>-0` — sid is 16 hex,
    // and the final pipe name fits in Linux's UDS sun_path budget
    // after the per-OS prefix). 12-char nonce stays safely under.
    let program = format!("scaffold-{:012x}", nonce & 0xFFFF_FFFF_FFFF);
    running_process::broker::protocol_v2::ServiceDefinitionBuilder::shared_broker(
        &program,
        stub_binary.display().to_string(),
    )
    .install_in(svc_dir.path())
    .expect("install stub servicedef");

    let path = env!("CARGO_BIN_EXE_running-process-broker-v2");
    let mut child = Command::new(path)
        .arg("--once")
        .arg("--program")
        .arg(&program)
        .env("RUNNING_PROCESS_SERVICE_DEF_DIR", svc_dir.path())
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

    // Dial the same pipe, run the full Hello / Negotiated round-trip,
    // and assert the binary echoes our connection_id back in its reply.
    let name = wrap_socket_name(&socket_path).expect("wrap_socket_name");
    let mut stream = Stream::connect(name).expect("connect to v2 broker pipe");

    use prost::Message;
    use running_process::broker::protocol::{
        hello_reply, read_frame, write_frame, Hello, HelloReply, ENVELOPE_VERSION,
    };

    let hello = Hello {
        client_min_protocol: ENVELOPE_VERSION as u32,
        client_max_protocol: ENVELOPE_VERSION as u32,
        service_name: program.clone(),
        wanted_version: "0.0.0".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        client_capabilities: 0,
        auth_token: Vec::new(),
        request_id: "slice-3d-integration-test".to_string(),
        connection_id: 0xdead_beef,
        peer_pid: std::process::id(),
        client_lib_name: "slice-3d-test".to_string(),
        client_lib_version: env!("CARGO_PKG_VERSION").to_string(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 0,
    };
    let mut body = Vec::with_capacity(hello.encoded_len());
    hello.encode(&mut body).expect("encode Hello");
    write_frame(&mut stream, &body).expect("write Hello frame");

    let reply_bytes = read_frame(&mut stream).expect("read HelloReply frame");
    let reply = HelloReply::decode(reply_bytes.as_slice()).expect("decode HelloReply");
    let negotiated = match reply.result {
        Some(hello_reply::Result::Negotiated(n)) => n,
        Some(hello_reply::Result::Refused(r)) => panic!("expected Negotiated, got Refused: {r:?}"),
        None => panic!("HelloReply.result missing"),
    };
    assert_eq!(negotiated.negotiated_protocol, ENVELOPE_VERSION as u32);
    assert_eq!(negotiated.connection_id, 0xdead_beef);
    assert!(
        !negotiated.daemon_version.is_empty(),
        "daemon_version should be populated"
    );

    drop(stream);

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
    assert!(
        all_stdout.contains("Hello for service"),
        "expected Hello-handler log line in stdout, got:\n{all_stdout}"
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
