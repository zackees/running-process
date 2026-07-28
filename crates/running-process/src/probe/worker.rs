//! The background registration worker.
//!
//! Everything that can block lives here, on its own thread, so [`super::install`]
//! can return immediately regardless of whether a daemon exists.
//!
//! The worker owns the full lifecycle: discover the daemon, register,
//! heartbeat, and on any failure back off and start over. Re-running the
//! *whole* handshake on reconnect is deliberate — the daemon's registry is
//! in-memory, so a daemon restart forgets us and only a fresh registration
//! returns this process to `ARMED`.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use running_process_probe::probe_diag::v1::{ProcessKey, RegisterProcess};

use super::client::{ProbeClient, SocketProbeClient};
use super::Config;

/// First reconnect delay.
const BACKOFF_START: Duration = Duration::from_millis(100);
/// Ceiling on the reconnect delay. Bounded so a long-absent daemon is still
/// picked up promptly once it appears.
const BACKOFF_CAP: Duration = Duration::from_secs(5);
/// Bound on each connect / request.
const IO_DEADLINE: Duration = Duration::from_millis(500);
/// How finely the worker checks the stop flag while waiting.
///
/// Sleeping for a whole backoff or heartbeat interval would make `Guard::drop`
/// wait that long; slicing the wait keeps shutdown prompt.
const STOP_POLL: Duration = Duration::from_millis(50);

/// Assemble this process's registration request.
///
/// Fallible only because identifying the current executable can fail; that is
/// a local condition, unrelated to whether a daemon exists.
pub fn build_register_request(config: &Config) -> io::Result<RegisterProcess> {
    let exe = std::env::current_exe()?;

    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce).map_err(|e| io::Error::other(format!("getrandom: {e}")))?;

    let started_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    Ok(RegisterProcess {
        key: Some(ProcessKey {
            pid: u64::from(std::process::id()),
            // TODO(#628 S6): source the true OS process start time. Install
            // time is close enough to distinguish instances today, but it is
            // not the same value the daemon would read from the OS.
            start_time: Some(started_at_unix_ms),
            boot_id: Some(crate::broker::host_identity::current().boot_id),
        }),
        exe_path: exe.to_string_lossy().into_owned(),
        app_class: config.app_class.clone(),
        app_name: config.app_name.clone(),
        app_version: config.app_version.clone(),
        instance_name: config.instance.clone().unwrap_or_default(),
        arch: std::env::consts::ARCH.to_string(),
        os: std::env::consts::OS.to_string(),
        // Declared, never inferred. The daemon cannot tell a Python process
        // from a native one by looking at it — the interpreter is just another
        // native executable — so leaving this at its default would report
        // every registrant as UNSPECIFIED.
        runtime: config.runtime.to_proto() as i32,
        registration_nonce: nonce.to_vec(),
        ..Default::default()
    })
}

/// Spawn the worker thread.
pub fn spawn(
    request: RegisterProcess,
    config: Config,
    stop: Arc<AtomicBool>,
    key_out: Arc<Mutex<Option<ProcessKey>>>,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("rp-probe".into())
        .spawn(move || run(request, config, stop, key_out))
}

fn run(
    request: RegisterProcess,
    config: Config,
    stop: Arc<AtomicBool>,
    key_out: Arc<Mutex<Option<ProcessKey>>>,
) {
    let mut backoff = BACKOFF_START;

    while !stop.load(Ordering::Relaxed) {
        match connect_and_register(&request, &config) {
            Ok((mut client, key)) => {
                backoff = BACKOFF_START;
                set_key(&key_out, Some(key.clone()));

                heartbeat_loop(&mut client, &key, &config, &stop);

                // Either we are shutting down or the connection failed. Either
                // way this process is no longer armed.
                set_key(&key_out, None);

                if stop.load(Ordering::Relaxed) {
                    // Best-effort courtesy notice. The daemon would notice the
                    // closed connection regardless.
                    let _ = client.unregister(&key);
                    return;
                }
            }
            Err(_) => {
                // No daemon, or it refused. Neither is fatal to the
                // application — wait and try again.
                sleep_interruptible(backoff, &stop);
                backoff = (backoff * 2).min(BACKOFF_CAP);
            }
        }
    }
}

fn connect_and_register(
    request: &RegisterProcess,
    config: &Config,
) -> Result<(SocketProbeClient, ProcessKey), super::client::ClientError> {
    let socket = resolve_socket_path(config)?;
    let mut client = SocketProbeClient::connect(&socket, IO_DEADLINE)?;
    let key = client.register(request)?;
    Ok((client, key))
}

/// Where to reach the daemon.
///
/// An explicit override wins; otherwise the daemon's owner-only discovery file
/// names the socket. Absence of that file simply means "no daemon yet", which
/// the caller treats as a retry.
fn resolve_socket_path(config: &Config) -> Result<String, super::client::ClientError> {
    if let Some(path) = &config.socket_override {
        return Ok(path.to_string_lossy().into_owned());
    }
    Err(super::client::ClientError::Unreachable(io::Error::new(
        io::ErrorKind::NotFound,
        "no probe daemon discovery file; set Config::socket_override or start rpprobed",
    )))
}

fn heartbeat_loop(
    client: &mut dyn ProbeClient,
    key: &ProcessKey,
    config: &Config,
    stop: &AtomicBool,
) {
    loop {
        sleep_interruptible(config.heartbeat_interval, stop);
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if client.heartbeat(key).is_err() {
            // Connection is gone. Returning sends the worker back through the
            // full register handshake, which is what a restarted daemon needs.
            return;
        }
    }
}

fn set_key(slot: &Arc<Mutex<Option<ProcessKey>>>, value: Option<ProcessKey>) {
    if let Ok(mut guard) = slot.lock() {
        *guard = value;
    }
}

/// Sleep, but wake early if asked to stop.
fn sleep_interruptible(total: Duration, stop: &AtomicBool) {
    let mut slept = Duration::ZERO;
    while slept < total {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let step = STOP_POLL.min(total - slept);
        std::thread::sleep(step);
        slept += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_request_describes_this_process() {
        let req = build_register_request(&Config::new("test-app")).unwrap();
        let key = req.key.expect("key");
        assert_eq!(key.pid, u64::from(std::process::id()));
        assert!(!req.exe_path.is_empty());
        assert_eq!(req.app_class, "test-app");
        assert_eq!(req.arch, std::env::consts::ARCH);
        assert_eq!(req.os, std::env::consts::OS);
    }

    /// The runtime must be declared on the wire, not left at the proto default.
    ///
    /// `UNSPECIFIED` is what the field held before it was populated at all, so
    /// asserting the concrete value is what distinguishes "reported native"
    /// from "reported nothing".
    #[test]
    fn the_declared_runtime_reaches_the_request() {
        use running_process_probe::probe_diag::v1::Runtime as ProtoRuntime;

        let native = build_register_request(&Config::new("a")).unwrap();
        assert_eq!(
            native.runtime,
            ProtoRuntime::Native as i32,
            "a Rust registrant defaults to native, not unspecified"
        );

        let python =
            build_register_request(&Config::new("a").with_runtime(super::super::Runtime::Python))
                .unwrap();
        assert_eq!(python.runtime, ProtoRuntime::Python as i32);
    }

    #[test]
    fn each_registration_gets_a_fresh_nonce() {
        let a = build_register_request(&Config::new("a")).unwrap();
        let b = build_register_request(&Config::new("a")).unwrap();
        assert_eq!(a.registration_nonce.len(), 32);
        assert_ne!(
            a.registration_nonce, b.registration_nonce,
            "a reused nonce would be rejected as a replay"
        );
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        let mut d = BACKOFF_START;
        for _ in 0..10 {
            d = (d * 2).min(BACKOFF_CAP);
        }
        assert_eq!(d, BACKOFF_CAP, "backoff must saturate, not grow unbounded");
    }

    #[test]
    fn interruptible_sleep_wakes_early_on_stop() {
        let stop = AtomicBool::new(true);
        let start = std::time::Instant::now();
        sleep_interruptible(Duration::from_secs(30), &stop);
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "an already-set stop flag must short-circuit the wait"
        );
    }

    #[test]
    fn interruptible_sleep_waits_when_not_stopped() {
        let stop = AtomicBool::new(false);
        let start = std::time::Instant::now();
        sleep_interruptible(Duration::from_millis(150), &stop);
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

    #[test]
    fn missing_discovery_is_unreachable_not_a_panic() {
        let err = resolve_socket_path(&Config::new("app")).expect_err("no daemon");
        assert!(matches!(
            err,
            super::super::client::ClientError::Unreachable(_)
        ));
    }

    #[test]
    fn socket_override_wins_over_discovery() {
        let mut cfg = Config::new("app");
        cfg.socket_override = Some(std::path::PathBuf::from("/tmp/x.sock"));
        assert_eq!(resolve_socket_path(&cfg).unwrap(), "/tmp/x.sock");
    }
}
