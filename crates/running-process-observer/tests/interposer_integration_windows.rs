//! Slice 7a end-to-end integration test (#551).
//!
//! Wires the slice 6 pieces together: build the interposer DLL,
//! spawn a real child process, inject the DLL via the slice 6d
//! injection vehicle, then assert that the detours installed in
//! `DllMain` actually fire — `RPO_HOOK …` lines must appear on
//! the child's stderr after the inject returns.
//!
//! This is the first test that exercises the full pipeline
//! end-to-end. Each prior slice has its own unit tests for the
//! pieces; slice 7 is the contract:
//!
//! 1. Building the interposer crate produces a loadable DLL.
//! 2. The injector loads it into a target.
//! 3. The detours installed in `DllMain` actually intercept
//!    subsequent file APIs in the target's address space.
//! 4. The intercepted events arrive on the child's stderr in the
//!    documented `RPO_HOOK` format.

#![cfg(all(feature = "embed-helper", target_os = "windows"))]

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use running_process_observer::inject_into_pid;

/// Locate the workspace `target/<triple>/<profile>/` directory the
/// current test binary was built into. The test binary lives at
/// `target/<triple>/<profile>/deps/<test>.exe`, so we walk up one
/// directory.
fn target_profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    exe.parent() // deps/
        .and_then(|p| p.parent()) // <profile>/
        .expect("walk up from test exe")
        .to_path_buf()
}

/// Build the Windows interposer DLL on demand. Returns its path.
///
/// Shells out to `cargo build -p running-process-observer-
/// interposer-windows`. The first run rebuilds; subsequent runs
/// are no-ops because cargo's incremental checks find no changes.
/// Either way we end up with the DLL at
/// `<target>/<profile>/running_process_observer_interposer_windows.dll`.
fn build_and_locate_interposer_dll() -> PathBuf {
    // Drive cargo through `soldr` if the rest of the workspace is
    // — matches the project's build rule (see CLAUDE.md). If soldr
    // isn't on PATH, fall back to bare `cargo`.
    let mut cmd = if which("soldr").is_some() {
        let mut c = Command::new("soldr");
        c.arg("cargo");
        c
    } else {
        Command::new("cargo")
    };
    let status = cmd
        .args([
            "build",
            "-p",
            "running-process-observer-interposer-windows",
        ])
        .status()
        .expect("spawn cargo to build interposer dll");
    assert!(
        status.success(),
        "cargo build of interposer DLL failed: {status:?}"
    );

    let dll = target_profile_dir()
        .join("running_process_observer_interposer_windows.dll");
    assert!(
        dll.exists(),
        "expected interposer DLL at {dll:?} after cargo build"
    );
    dll
}

/// Lightweight `which` replacement for Windows. Returns the first
/// matching path in `PATH` if `name` (or `name.exe`) resolves.
fn which(name: &str) -> Option<PathBuf> {
    let candidates: Vec<String> = if name.ends_with(".exe") {
        vec![name.to_string()]
    } else {
        vec![name.to_string(), format!("{name}.exe")]
    };
    let path = std::env::var_os("PATH")?;
    for entry in std::env::split_paths(&path) {
        for cand in &candidates {
            let p = entry.join(cand);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

// Slice 7b: DllMain now defers the retour install to a worker
// thread spawned via CreateThread. This unblocks the test that
// previously hung when injecting into cmd.exe (the loader-lock
// re-entrance issue is documented in the DLL's DllMain Safety
// docs). The test gives the worker a short grace period after
// inject_into_pid returns before exercising any detoured API.
#[test]
fn interposer_dll_fires_rpo_hook_after_inject() {
    let dll = build_and_locate_interposer_dll();

    // Probe file the child will `type`-open after the inject.
    // Write our own tempfile with a short bareword name and run
    // cmd with cwd set to the tempdir. That sidesteps cmd's
    // path-with-quotes parsing (and Rust's argv-to-cmdline
    // escaping pass) entirely.
    let tmp = tempfile::tempdir().expect("tempdir");
    let probe_name = "probe.txt";
    let probe_path = tmp.path().join(probe_name);
    std::fs::write(&probe_path, b"hello from slice 7\n").expect("write probe");

    // The ping gives the inject worker thread 2+ seconds to
    // install detours before the `type` triggers CreateFileW
    // under them.
    let cmd_line = format!(
        "ping -n 3 127.0.0.1 > nul & type {}",
        probe_name
    );

    let mut child = Command::new("cmd")
        .arg("/c")
        .arg(&cmd_line)
        .current_dir(tmp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cmd ping+type");
    let pid = child.id();

    // Give cmd a moment to start its first child (the ping). We
    // want to inject WHILE ping is sleeping, so the subsequent
    // `type` runs with the interposer already attached.
    std::thread::sleep(Duration::from_millis(200));

    let inject_result = inject_into_pid(pid, &dll);

    // Slice 7b: DllMain returns immediately, then a worker thread
    // installs the retour detours off the loader lock. Give that
    // thread time to finish before we expect any RPO_HOOK output.
    // 200 ms is comfortably more than the empirical worst case
    // (retour install measures ~30 ms per detour × 5 detours, with
    // VirtualProtect overhead).
    std::thread::sleep(Duration::from_millis(200));

    // Drain stderr on a background thread so the main thread can
    // enforce a wall-clock deadline. `Child::stderr.read()` is
    // synchronous + blocks until the child writes or closes; if we
    // ran it inline the deadline check would only fire between
    // reads, and a stuck child would block us until the next byte
    // arrives.
    let stderr_text: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let stderr_pipe = child.stderr.take().expect("stderr piped");
    let reader_text = Arc::clone(&stderr_text);
    let reader = std::thread::spawn(move || {
        let mut pipe = stderr_pipe;
        let mut buf = [0u8; 4096];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut s) = reader_text.lock() {
                        s.push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Wait until we observe an `RPO_HOOK` line OR the deadline
    // hits. Polling Mutex is fine — the contention window is sub-
    // microsecond and we sleep 50 ms between polls.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if stderr_text
            .lock()
            .map(|s| s.contains("RPO_HOOK"))
            .unwrap_or(false)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Tear the child down so the reader thread exits.
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();

    let hmodule = inject_result.expect("inject_into_pid should succeed");
    assert!(hmodule != 0, "remote LoadLibraryW returned NULL");

    // The interposer's detours emit one or more `RPO_HOOK ...`
    // lines from inside cmd's address space (its CreateFileW
    // calls for the `type` builtin, plus any incidental file I/O
    // cmd.exe does itself). We just need to see one.
    let captured = stderr_text.lock().map(|s| s.clone()).unwrap_or_default();
    assert!(
        captured.contains("RPO_HOOK"),
        "expected at least one RPO_HOOK line on the child's stderr; \
         got: {captured:?}"
    );
}
