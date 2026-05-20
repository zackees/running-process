//! Hang-watchdog for tests.
//!
//! Drop the returned [`WatchdogGuard`] before the timeout expires to cancel;
//! otherwise the watcher thread fires and:
//!
//! - **Windows**: invokes `procdump -ma <pid>` to write a full minidump
//!   (all threads, full memory), prints `message` plus the actual dump
//!   path, then `std::process::exit(1)`.
//! - **Other platforms**: no-op. The watcher thread is still spawned so
//!   the same API works cross-platform, but nothing happens on timeout.
//!
//! Cancel happens via channel-disconnect when the guard's `Sender` drops,
//! so normal returns AND panic unwinds both cancel cleanly.
//!
//! # Usage
//!
//! ```no_run
//! use std::time::Duration;
//!
//! #[test]
//! fn slow_test() {
//!     let _wd = test_watchdog::install(
//!         Duration::from_secs(30),
//!         "slow_test appears to be hung",
//!         None, // auto-generate dump path in env::temp_dir()
//!     );
//!     // ... test body ...
//! }
//! ```

use std::path::PathBuf;
use std::time::Duration;

/// Cancel handle for a watchdog. Drop it to cancel the watcher.
pub struct WatchdogGuard {
    // The watcher thread is the only consumer; dropping this Sender
    // produces a `RecvTimeoutError::Disconnected` which the thread
    // treats as "cancelled, exit silently".
    #[allow(dead_code)]
    inner: GuardInner,
}

enum GuardInner {
    #[allow(dead_code)]
    Noop,
    #[cfg(windows)]
    #[allow(dead_code)]
    Active(std::sync::mpsc::Sender<()>),
}

/// Install a hang watchdog.
///
/// `timeout` — how long to wait before firing.
/// `message` — prefix printed to stderr when the watchdog fires (callers
/// pass something like `"<test name> appears to be hung"`).
/// `dump_path` — explicit minidump location (Windows-only effect). `None`
/// auto-generates a path in `env::temp_dir()`.
pub fn install(
    timeout: Duration,
    message: impl Into<String>,
    dump_path: Option<PathBuf>,
) -> WatchdogGuard {
    #[cfg(windows)]
    {
        install_windows(timeout, message.into(), dump_path)
    }
    #[cfg(not(windows))]
    {
        let _ = (timeout, message, dump_path);
        WatchdogGuard {
            inner: GuardInner::Noop,
        }
    }
}

#[cfg(windows)]
fn install_windows(
    timeout: Duration,
    message: String,
    dump_path: Option<PathBuf>,
) -> WatchdogGuard {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let pid = std::process::id();
    let dump_path = dump_path.unwrap_or_else(|| {
        std::env::temp_dir().join(format!("test-watchdog-pid{pid}.dmp"))
    });
    std::thread::Builder::new()
        .name("test-watchdog".to_string())
        .spawn(move || match rx.recv_timeout(timeout) {
            Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                fire(pid, timeout, &message, &dump_path);
            }
        })
        .expect("spawn watchdog thread");
    WatchdogGuard {
        inner: GuardInner::Active(tx),
    }
}

#[cfg(windows)]
fn fire(pid: u32, after: Duration, message: &str, dump_path: &std::path::Path) -> ! {
    eprintln!(
        "{message} — stack dump because process appears to be hung \
         (no progress in {after:?}); invoking procdump on PID {pid}"
    );
    let output = std::process::Command::new("procdump")
        .args(["-accepteula", "-ma", &pid.to_string()])
        .arg(dump_path)
        .output();
    match output {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            eprintln!("procdump exit: {}", o.status);
            if !stdout.is_empty() {
                eprintln!("procdump stdout:\n{stdout}");
            }
            if !stderr.is_empty() {
                eprintln!("procdump stderr:\n{stderr}");
            }
            // procdump prints `Dump 1 initiated: <full_path>` — grab the
            // actual on-disk path (procdump sometimes rewrites the name
            // we asked for).
            let actual = stdout
                .lines()
                .find_map(|l| l.split_once("Dump 1 initiated:").map(|(_, p)| p.trim()))
                .map(PathBuf::from);
            match actual {
                Some(p) if p.exists() => {
                    eprintln!("minidump written: {}", p.display());
                    eprintln!(
                        "open in WinDbg / Visual Studio: `~* k` for all-thread callstacks"
                    );
                }
                Some(p) => {
                    eprintln!("procdump reported {} but file is missing", p.display());
                }
                None => {
                    eprintln!("could not parse dump path from procdump output");
                }
            }
        }
        Err(e) => {
            eprintln!("failed to invoke procdump: {e}");
            eprintln!(
                "(install procdump from sysinternals or place it on PATH \
                 to capture a thread dump on hang)"
            );
        }
    }
    std::process::exit(1);
}
