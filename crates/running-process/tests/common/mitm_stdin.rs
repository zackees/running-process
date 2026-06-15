//! Stdin-side MITM helper used by the substrate tests in #448 / #449.
//!
//! Wraps a `NativePtyProcess` running one of the `testbin-stdin-*` or
//! `testbin-paste-*` fixtures. Exposes `write_stdin`, drain helpers,
//! and an opinionated `assert_received_exact` that fails fast with
//! a hex diff when the child's echoed bytes drift from expected.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use running_process::pty::NativePtyProcess;

/// Returns `true` if the host PTY substrate is expected to be
/// byte-exact under `PSEUDOCONSOLE_PASSTHROUGH_MODE` on the current
/// platform.
///
/// Always `true` on POSIX (native PTY is byte-exact by design).
/// On Windows 11 / Server 2022 build >= 22000, native kernel32 ConPTY
/// honors PASSTHROUGH_MODE so `true`. On Windows 10 (build < 22000),
/// passthrough only works when the Microsoft.Windows.Console.ConPTY
/// redistributable's `conpty.dll` is staged either next to the test
/// binary or in this crate's auto-acquire cache (#445/#446).
///
/// Tests call this and `return` early when it's `false`, mirroring
/// `daemon_tui_repaint_test`'s skip pattern. CI logs report
/// "SKIPPED:" so the run remains green on unsupported hosts.
pub fn mitm_byte_exact_supported() -> bool {
    #[cfg(not(windows))]
    {
        true
    }
    #[cfg(windows)]
    {
        let build = windows_build_number().unwrap_or(0);
        if build >= 22000 {
            return true;
        }
        sidecar_conpty_dll_present()
    }
}

/// Print a uniform skip line. Use at the top of every test that
/// asserts byte-exact MITM behavior.
pub fn skip_unless_mitm_supported() -> bool {
    if mitm_byte_exact_supported() {
        return false;
    }
    eprintln!(
        "[SKIP] byte-exact MITM substrate requires Windows 11+ (build >= 22000) \
         or a cached Win10 ConPTY sidecar (#446). Current host: {}",
        host_description()
    );
    true
}

#[cfg(windows)]
fn host_description() -> String {
    format!(
        "Windows build {}, sidecar present = {}",
        windows_build_number().unwrap_or(0),
        sidecar_conpty_dll_present()
    )
}

#[cfg(not(windows))]
fn host_description() -> String {
    "POSIX".to_string()
}

#[cfg(windows)]
fn windows_build_number() -> Option<u32> {
    let output = Command::new("cmd").args(["/c", "ver"]).output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v = stdout.split('.').nth(2)?;
    v.parse::<u32>().ok()
}

#[cfg(windows)]
fn sidecar_conpty_dll_present() -> bool {
    // 1. Next to the test binary (admin pre-stage path).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            if parent.join("conpty.dll").is_file() {
                return true;
            }
        }
    }
    // 2. Auto-acquire cache from #446 (mirrors the runtime layout).
    let Some(cache_root) = dirs::cache_dir() else {
        return false;
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86") {
        "x86"
    } else if cfg!(target_arch = "arm") {
        "arm"
    } else {
        return false;
    };
    let p = cache_root
        .join("running-process")
        .join("conpty")
        .join(env!("CARGO_PKG_VERSION"))
        .join(arch)
        .join("conpty.dll");
    p.is_file()
}

/// Build (or locate, if already built) the named testbin and return
/// its absolute path. Uses `cargo build --message-format=json` to
/// resolve the artifact path in a way that survives target dir
/// overrides.
pub fn testbin_path(name: &str) -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "testbins",
            "--bin",
            name,
            "--message-format=json",
        ])
        .stderr(std::process::Stdio::inherit())
        .output()
        .unwrap_or_else(|e| panic!("cargo build for testbin {name} failed: {e}"));
    assert!(
        output.status.success(),
        "cargo build -p testbins --bin {name} returned non-zero status"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if !line.contains("\"compiler-artifact\"") || !line.contains(name) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["reason"] != "compiler-artifact" {
            continue;
        }
        let Some(kinds) = v["target"]["kind"].as_array() else {
            continue;
        };
        if !kinds.iter().any(|k| k == "bin") {
            continue;
        }
        if let Some(exe) = v["executable"].as_str() {
            let p = PathBuf::from(exe);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !p.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            return p;
        }
    }
    panic!("could not locate compiler-artifact for testbin {name}");
}

/// Round-trip session for stdin-side MITM tests. Holds the
/// `NativePtyProcess`, exposes write + drain primitives, and tears
/// down cleanly on `Drop`.
///
/// The `prefetched` buffer captures bytes the spawn-time handshake
/// over-drained past the ACK marker. Subsequent reads consume them
/// before pulling new bytes off the master pipe, so tests that
/// inspect startup-time child writes (e.g. the `--advertise-paste`
/// flow in #449 test 8) still see them.
pub struct EchoerSession {
    process: NativePtyProcess,
    prefetched: Mutex<VecDeque<u8>>,
}

impl EchoerSession {
    /// Spawn `testbin-stdin-echoer` with the given argv tail and
    /// wait for its startup ACK byte (`\x06`).
    ///
    /// The handshake fences against the POSIX line-discipline race:
    /// without it, the host's first `write_stdin` could race the
    /// testbin's `cfmakeraw` and trigger cooked-mode echo
    /// (`\x1b` → `^[`). Bytes the testbin wrote *after* the ACK
    /// (e.g. the bracketed-paste enable sequence when
    /// `--advertise-paste` is set) are retained in the session's
    /// `prefetched` buffer and consumed by subsequent reads.
    ///
    /// `args` are appended to the testbin invocation (e.g.
    /// `["--advertise-paste"]` or `["--exit-on", "0x04"]`).
    pub fn spawn(args: &[&str]) -> Self {
        let bin = testbin_path("testbin-stdin-echoer");
        let mut argv: Vec<String> = Vec::with_capacity(1 + args.len());
        argv.push(bin.to_string_lossy().into_owned());
        for a in args {
            argv.push((*a).to_string());
        }
        let process = NativePtyProcess::new(argv, None, None, 24, 80, None)
            .expect("construct NativePtyProcess");
        process.start_impl().expect("start NativePtyProcess");
        let session = Self {
            process,
            prefetched: Mutex::new(VecDeque::new()),
        };
        let handshake = Duration::from_secs(5);
        let drained = session.drain_raw_until_byte(0x06, handshake);
        let ack_pos = drained.iter().position(|&b| b == 0x06).unwrap_or_else(|| {
            panic!(
                "testbin-stdin-echoer never emitted startup ACK in {handshake:?}; \
                 drained {} bytes: {:02x?}",
                drained.len(),
                drained
            )
        });
        // Retain everything *after* the ACK for the next test-visible
        // read. The ACK itself is the handshake marker and is dropped.
        if ack_pos + 1 < drained.len() {
            session
                .prefetched
                .lock()
                .expect("prefetched mutex poisoned")
                .extend(drained[ack_pos + 1..].iter().copied());
        }
        session
    }

    /// Internal: raw read from the master pipe until either `target`
    /// byte appears or `timeout` elapses. Unlike the public
    /// `drain_until_contains`, this does NOT consult the prefetched
    /// buffer — used at spawn time before prefetched has anything in
    /// it.
    fn drain_raw_until_byte(&self, target: u8, timeout: Duration) -> Vec<u8> {
        let mut out = Vec::new();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let slice = remaining.min(Duration::from_millis(100));
            match self.process.read_chunk_impl(Some(slice.as_secs_f64())) {
                Ok(Some(bytes)) => {
                    out.extend_from_slice(&bytes);
                    if out.iter().any(|&b| b == target) {
                        return out;
                    }
                }
                Ok(None) => {}
                Err(e) => panic!("read_chunk_impl error: {e:?}"),
            }
        }
        out
    }

    /// Drain bytes from the prefetched buffer (max `max_bytes`).
    /// Returns whatever was queued; never blocks.
    fn drain_prefetched(&self, max_bytes: usize) -> Vec<u8> {
        let mut guard = self.prefetched.lock().expect("prefetched mutex poisoned");
        let take = max_bytes.min(guard.len());
        let mut out = Vec::with_capacity(take);
        for _ in 0..take {
            if let Some(b) = guard.pop_front() {
                out.push(b);
            }
        }
        out
    }

    /// Write `data` to the child's stdin in one syscall. Does not
    /// flag the write as a "submit" event (that's a PTY-input-metrics
    /// concept and not relevant to the byte-exact guarantees we test
    /// here).
    pub fn write_stdin(&self, data: &[u8]) {
        self.process
            .write_impl(data, false)
            .unwrap_or_else(|e| panic!("write_stdin({} bytes) failed: {e:?}", data.len()));
    }

    /// Read bytes from the child's stdout for up to `timeout`,
    /// returning the concatenated bytes seen. Drains the spawn-time
    /// prefetched buffer before pulling fresh bytes off the master
    /// pipe. Returns early once `target_len` bytes have arrived (if
    /// `Some`).
    pub fn drain_for(&self, timeout: Duration, target_len: Option<usize>) -> Vec<u8> {
        let mut out = self.drain_prefetched(target_len.unwrap_or(usize::MAX));
        if let Some(target) = target_len {
            if out.len() >= target {
                return out;
            }
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let slice = remaining.min(Duration::from_millis(100));
            let chunk = self
                .process
                .read_chunk_impl(Some(slice.as_secs_f64()))
                .expect("read_chunk_impl");
            match chunk {
                Some(bytes) => {
                    out.extend_from_slice(&bytes);
                    if let Some(target) = target_len {
                        if out.len() >= target {
                            return out;
                        }
                    }
                }
                None => {
                    // No data; spin on the timeout.
                    if Instant::now() >= deadline {
                        break;
                    }
                }
            }
        }
        out
    }

    /// Drain stdout for up to `timeout` and assert the child wrote
    /// back `expected`, byte-exact. On mismatch panics with a
    /// hex-rendered diff to make tearing or translation visible.
    pub fn assert_received_exact(&self, expected: &[u8], timeout: Duration) {
        let got = self.drain_for(timeout, Some(expected.len()));
        if got.as_slice() != expected {
            panic!(
                "byte-exact mismatch:\n  expected ({} bytes): {}\n  got      ({} bytes): {}",
                expected.len(),
                hex(expected),
                got.len(),
                hex(&got),
            );
        }
    }

    /// Drain stdout until either `needle` is contained or `timeout`
    /// elapses. Drains the prefetched buffer first, then reads from
    /// the master pipe. Returns the full drained buffer for further
    /// inspection.
    pub fn drain_until_contains(&self, needle: &[u8], timeout: Duration) -> Vec<u8> {
        let mut out = self.drain_prefetched(usize::MAX);
        if !out.is_empty() && out.windows(needle.len()).any(|w| w == needle) {
            return out;
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let slice = remaining.min(Duration::from_millis(100));
            match self.process.read_chunk_impl(Some(slice.as_secs_f64())) {
                Ok(Some(bytes)) => {
                    out.extend_from_slice(&bytes);
                    if out.windows(needle.len()).any(|w| w == needle) {
                        return out;
                    }
                }
                Ok(None) => {}
                Err(e) => panic!("read_chunk_impl error: {e:?}"),
            }
        }
        out
    }

    /// Drop the child's stdin pipe, signalling EOF. Used by tests
    /// that need a deterministic teardown without sending a control
    /// byte.
    pub fn close_stdin(&self) {
        // `close_impl` tears down the whole process; for an EOF-only
        // signal we don't have a dedicated API. Tests that need the
        // distinction rely on `--exit-on <byte>` instead.
        let _ = self.process.close_impl();
    }

    pub fn process(&self) -> &NativePtyProcess {
        &self.process
    }
}

impl Drop for EchoerSession {
    fn drop(&mut self) {
        // Best-effort teardown. Each test owns its own session, so a
        // failure here means CI logs get extra noise but never a
        // false negative.
        let _ = self.process.kill_impl();
        let _ = self.process.wait_impl(Some(1.5));
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && i % 16 == 0 {
            s.push('\n');
            s.push_str("                                ");
        }
        s.push_str(&format!("{b:02x} "));
    }
    s
}
