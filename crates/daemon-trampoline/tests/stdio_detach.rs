//! Regression test for stdio detach in `daemon-trampoline`.
//!
//! `launch_detached(...)` spawns this trampoline, which in turn spawns the
//! user's long-lived command. If the trampoline inherits and retains
//! stdin/stdout/stderr from its caller, every grandparent process that
//! reads those pipes (e.g. `subprocess.Popen(stdout=PIPE)`) hangs
//! indefinitely after the immediate caller exits — the orphaned trampoline
//! + child keep the write ends alive.
//!
//! Test strategy: copy the trampoline binary into a tempdir under a fresh
//! name, write a `<stem>.daemon.json` sidecar that points at a
//! long-running platform-native sleep command, and spawn the trampoline
//! with `Stdio::piped()` for stdin/stdout/stderr. We hold the read ends;
//! the trampoline (and its child) inherit the write ends.
//!
//! With the fix, `detach_stdio()` runs *before* the child is spawned;
//! both write ends are released and our `read_to_end` on the child's
//! stdout/stderr returns EOF within milliseconds. Without the fix the
//! reads block until the trampoline's child exits (we kill it via
//! `child.kill()` after a 10 s timeout, which counts as a failure).
//!
//! See issue #108.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Platform-native long-sleep command. The trampoline's child must outlive
/// the test's read timeout, so that without the fix the read genuinely
/// hangs (rather than EOF'ing because the child exited on its own).
#[cfg(unix)]
fn long_sleep_command() -> (&'static str, Vec<&'static str>) {
    ("sleep", vec!["30"])
}

#[cfg(windows)]
fn long_sleep_command() -> (&'static str, Vec<&'static str>) {
    // `ping -n N` waits ~(N-1) seconds; redirect chatter so it would write
    // to the inherited pipe (and thereby keep it alive) without the fix.
    ("cmd", vec!["/C", "ping -n 31 127.0.0.1 > NUL"])
}

#[test]
fn trampoline_releases_inherited_stdio_before_spawning_child() {
    let trampoline_src = env!("CARGO_BIN_EXE_daemon-trampoline");
    let tmp = tempfile::tempdir().expect("create tempdir");

    // The trampoline derives its sidecar from its own exe stem, so isolate
    // each test run with a fresh exe name in the tempdir.
    let trampoline_dest = if cfg!(windows) {
        tmp.path().join("test-trampoline.exe")
    } else {
        tmp.path().join("test-trampoline")
    };
    std::fs::copy(trampoline_src, &trampoline_dest).expect("copy trampoline");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&trampoline_dest).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&trampoline_dest, perms).unwrap();
    }

    let (cmd, args) = long_sleep_command();
    let sidecar_path = tmp.path().join("test-trampoline.daemon.json");
    let sidecar_json = serde_json::json!({
        "command": cmd,
        "args": args,
    });
    std::fs::write(&sidecar_path, sidecar_json.to_string()).expect("write sidecar");

    let mut child = Command::new(&trampoline_dest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn trampoline");

    let stdout = child.stdout.take().expect("take stdout");
    let stderr = child.stderr.take().expect("take stderr");

    let (tx_out, rx_out) = mpsc::channel::<()>();
    thread::spawn(move || {
        let mut sink = stdout;
        let mut buf = Vec::new();
        let _ = sink.read_to_end(&mut buf);
        let _ = tx_out.send(());
    });

    let (tx_err, rx_err) = mpsc::channel::<()>();
    thread::spawn(move || {
        let mut sink = stderr;
        let mut buf = Vec::new();
        let _ = sink.read_to_end(&mut buf);
        let _ = tx_err.send(());
    });

    // The trampoline's detach_stdio() runs at the top of run(); reaching
    // it is a few hundred microseconds. 10 s is generous enough for slow
    // CI yet short enough that a hang is unambiguous.
    let timeout = Duration::from_secs(10);
    let stdout_eof = rx_out.recv_timeout(timeout).is_ok();
    let stderr_eof = rx_err.recv_timeout(timeout).is_ok();

    // Tear down before asserting so we never leave a trampoline behind.
    // The trampoline's long-sleep child is a *separate* process and will
    // self-clean within ~30 s (deliberately bounded) on Windows where we
    // can't reliably kill the process tree from Rust stdlib alone.
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        stdout_eof,
        "trampoline did not close its inherited stdout within {timeout:?}; \
         a grandparent process reading this pipe would hang until the \
         trampoline's child exited. See issue #108."
    );
    assert!(
        stderr_eof,
        "trampoline did not close its inherited stderr within {timeout:?}; \
         a grandparent process reading this pipe would hang until the \
         trampoline's child exited. See issue #108."
    );
}
