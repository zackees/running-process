use super::*;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::mpsc::channel;

fn testbin_path(name: &str) -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    let profile_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    let path = profile_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "test fixture is missing at {} — run `soldr cargo build -p testbins` first",
        path.display()
    );
    path
}

/// Reassemble a proxied session's frame stream back into the (stdout, stderr,
/// exit) triple — the shape the direct oracle produces, so the two are
/// comparable byte-for-byte.
#[derive(Debug, PartialEq, Eq)]
struct Reassembled {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: (i32, i32),
}

/// Run the fixture through the pump: spawn it with piped stdio, feed `stdin`
/// (then EOF), drive `run_child_session`, and reassemble the emitted frames.
fn run_through_pump(directives: &[&str], stdin: &[u8]) -> Reassembled {
    let child = Command::new(testbin_path("testbin-stdio-scripted"))
        .args(directives)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fixture under pump");

    let (stdin_tx, stdin_rx) = channel();
    if !stdin.is_empty() {
        stdin_tx
            .send(SessionFrame {
                kind: Some(session_frame::Kind::Stdin(stdin.to_vec())),
            })
            .unwrap();
    }
    stdin_tx
        .send(SessionFrame {
            kind: Some(session_frame::Kind::StdinEof(true)),
        })
        .unwrap();
    drop(stdin_tx); // close the inbound lane

    let (out_tx, out_rx) = channel();
    let exit = run_child_session(child, out_tx, stdin_rx).expect("pump reaps child");

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_frame = None;
    for frame in out_rx {
        match frame.kind {
            Some(session_frame::Kind::Stdout(b)) => stdout.extend_from_slice(&b),
            Some(session_frame::Kind::Stderr(b)) => stderr.extend_from_slice(&b),
            Some(session_frame::Kind::Exit(e)) => exit_frame = Some((e.code, e.signal)),
            _ => panic!("unexpected inbound-only frame on the outbound lane"),
        }
    }
    let exit_frame = exit_frame.expect("a terminal Exit frame is always sent");
    assert_eq!(
        exit_frame,
        (exit.code, exit.signal),
        "the Exit frame must match the returned SessionExit"
    );
    Reassembled {
        stdout,
        stderr,
        exit: exit_frame,
    }
}

/// Run the fixture directly (the oracle/golden) and capture the same triple.
fn run_direct(directives: &[&str], stdin: &[u8]) -> Reassembled {
    let mut child = Command::new(testbin_path("testbin-stdio-scripted"))
        .args(directives)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fixture directly");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let output = child.wait_with_output().unwrap();
    Reassembled {
        stdout: output.stdout,
        stderr: output.stderr,
        exit: (output.status.code().unwrap_or(-1), 0),
    }
}

#[test]
fn pump_reproduces_direct_stdout_stderr_and_exit() {
    let script = &["out:ARTIFACT", "err:DIAGNOSTIC", "out:MORE", "exit:5"];
    assert_eq!(run_through_pump(script, b""), run_direct(script, b""));
}

#[test]
fn pump_keeps_stdout_and_stderr_on_their_own_streams() {
    // Interleaved writes must not cross streams once proxied.
    let script = &["out:1", "err:2", "out:3", "err:4", "out:5"];
    let pumped = run_through_pump(script, b"");
    assert_eq!(pumped.stdout, b"135");
    assert_eq!(pumped.stderr, b"24");
    assert_eq!(pumped, run_direct(script, b""));
}

#[test]
fn pump_is_byte_transparent_for_raw_non_utf8() {
    let script = &["outhex:00ff80", "errhex:8081ff"];
    let pumped = run_through_pump(script, b"");
    assert_eq!(pumped.stdout, vec![0x00, 0xff, 0x80]);
    assert_eq!(pumped.stderr, vec![0x80, 0x81, 0xff]);
    assert_eq!(pumped, run_direct(script, b""));
}

#[test]
fn pump_delivers_stdin_to_the_child_verbatim() {
    let payload: Vec<u8> = (0u8..=255).collect();
    let pumped = run_through_pump(&["echo"], &payload);
    assert_eq!(pumped.stdout, payload);
    assert_eq!(pumped, run_direct(&["echo"], &payload));
}

#[test]
fn pump_reports_a_nonzero_exit_code() {
    let pumped = run_through_pump(&["err:boom", "exit:7"], b"");
    assert_eq!(pumped.exit, (7, 0));
    assert_eq!(pumped.stderr, b"boom");
}
