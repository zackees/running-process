//! Phase 3 stdio-fidelity oracle (soldr#2365, slice 1).
//!
//! Phase 3 makes compile bytes flow client -> broker -> daemon with the broker
//! proxying every byte, replacing today's handoff-to-direct-connection model.
//! The acceptance contract for that rewrite is **byte-exact stdio fidelity**:
//! stdout and stderr passed through independently and losslessly (rustc emits
//! artifacts on stdout and JSON diagnostics on stderr — the two must never
//! cross or mutate), and exit codes preserved for success, failure, and signal
//! death.
//!
//! This file establishes that oracle *before* the proxy exists (the safe first
//! slice): it pins the fidelity contract against direct execution of the
//! `testbin-stdio-scripted` fixture. Later Phase 3 slices reuse [`Captured`] and
//! [`Captured::assert_matches`] to prove the proxy path reproduces the direct
//! (golden) behavior exactly. Today "subject" == "direct", which validates the
//! fixture + capture harness and freezes the goldens every proxy slice targets.

use std::io::Write;
use std::process::{Command, Stdio};

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

/// Byte-exact capture of one process run: the stdio-fidelity oracle's unit of
/// comparison. Later Phase 3 slices capture the proxy path into the same shape
/// and call [`Captured::assert_matches`] against the direct golden.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Captured {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: Option<i32>,
}

impl Captured {
    /// Assert this capture is byte-exact-identical to `golden` on both streams
    /// and the exit code — the fidelity contract itself.
    fn assert_matches(&self, golden: &Captured) {
        assert_eq!(
            self.stdout, golden.stdout,
            "stdout diverged: got {:?}, golden {:?}",
            self.stdout, golden.stdout
        );
        assert_eq!(
            self.stderr, golden.stderr,
            "stderr diverged: got {:?}, golden {:?}",
            self.stderr, golden.stderr
        );
        assert_eq!(
            self.exit_code, golden.exit_code,
            "exit code diverged: got {:?}, golden {:?}",
            self.exit_code, golden.exit_code
        );
    }
}

/// Run the fixture directly with `directives`, feeding `stdin`, and capture
/// stdout/stderr bytes + exit code. This is the oracle: direct execution is the
/// golden every proxy slice must reproduce.
fn run_direct(directives: &[&str], stdin: &[u8]) -> Captured {
    let mut child = Command::new(testbin_path("testbin-stdio-scripted"))
        .args(directives)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn testbin-stdio-scripted");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(stdin)
        .expect("write child stdin");
    let output = child.wait_with_output().expect("collect fixture output");
    Captured {
        stdout: output.stdout,
        stderr: output.stderr,
        exit_code: output.status.code(),
    }
}

#[test]
fn stdout_is_byte_exact() {
    let got = run_direct(&["out:hello world"], b"");
    got.assert_matches(&Captured {
        stdout: b"hello world".to_vec(),
        stderr: Vec::new(),
        exit_code: Some(0),
    });
}

#[test]
fn stdout_and_stderr_are_independent() {
    let got = run_direct(&["out:ARTIFACT", "err:DIAGNOSTIC"], b"");
    got.assert_matches(&Captured {
        stdout: b"ARTIFACT".to_vec(),
        stderr: b"DIAGNOSTIC".to_vec(),
        exit_code: Some(0),
    });
}

#[test]
fn interleaved_writes_never_cross_streams() {
    // rustc interleaves artifact stdout with JSON-diagnostic stderr; the two
    // must stay on their own stream regardless of write ordering.
    let got = run_direct(&["out:1", "err:2", "out:3", "err:4", "out:5"], b"");
    got.assert_matches(&Captured {
        stdout: b"135".to_vec(),
        stderr: b"24".to_vec(),
        exit_code: Some(0),
    });
}

#[test]
fn nonzero_exit_code_is_preserved() {
    let got = run_direct(&["err:boom", "exit:7"], b"");
    got.assert_matches(&Captured {
        stdout: Vec::new(),
        stderr: b"boom".to_vec(),
        exit_code: Some(7),
    });
}

#[test]
fn raw_non_utf8_bytes_survive_verbatim() {
    // Proxying must be byte-transparent, not text-normalizing: NUL, 0xFF, and a
    // lone 0x80 continuation byte must pass through unchanged on both streams.
    let got = run_direct(&["outhex:00ff80", "errhex:8081ff"], b"");
    got.assert_matches(&Captured {
        stdout: vec![0x00, 0xff, 0x80],
        stderr: vec![0x80, 0x81, 0xff],
        exit_code: Some(0),
    });
}

#[test]
fn stdin_is_delivered_to_the_child_verbatim() {
    // The dumb-terminal client feeds stdin through; prove the fixture receives
    // arbitrary bytes intact so later slices can assert the proxy does too.
    let payload: Vec<u8> = (0u8..=255).collect();
    let got = run_direct(&["echo"], &payload);
    got.assert_matches(&Captured {
        stdout: payload,
        stderr: Vec::new(),
        exit_code: Some(0),
    });
}
