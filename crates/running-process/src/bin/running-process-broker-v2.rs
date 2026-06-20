//! v2 broker binary — production accept loop + ServiceDefinitionLoader
//! integration (running-process#532 slice 1).
//!
//! Replaces the slice-3c scaffold (`SCAFFOLD_PROGRAM` + single
//! `accept()` + exit) with a real broker:
//!
//! 1. **`--program <name>`** CLI arg names the v2 pipe namespace
//!    (`rpb-v2-<program>-<sid_hash>-0`). Defaults to
//!    `broker-v2-scaffold` for backwards compatibility with the
//!    earlier integration tests.
//! 2. **Persistent accept loop** — each accepted connection spawns
//!    a thread that handles the Hello round-trip. The accept loop
//!    is bounded only by the OS's pending-connection backlog;
//!    in-flight handlers are bounded by the OS thread cap.
//! 3. **ServiceDefinitionLoader integration** — on each Hello, look
//!    up `hello.service_name` via the default v2 service-definition
//!    directory ([`ServiceDefinitionLoader::default_root`]). Reject
//!    unknown services with `ErrorServiceUnknown`; reject
//!    out-of-policy versions with `ErrorVersionBlocked` (mirrors
//!    v1's `hello_router::refused_from_version_policy`).
//! 4. **Adopt-stub** — replies `Negotiated { backend_pipe: "" }` for
//!    successful Hellos. Real backend-pipe resolution (read the
//!    daemon's IPC endpoint from its `BackendIdentity` sidecar +
//!    forward the adopt traffic) is a follow-up slice; this slice
//!    proves the discovery + version-policy contract end-to-end.
//!
//! Flags:
//! - `--no-bind`: skip the bind entirely; exit 0 (kept for the
//!   slice-3c integration test).
//! - `--once`: accept exactly one connection then exit (testing
//!   convenience; the persistent loop is the default).
//! - `--program <name>`: name the v2 pipe namespace. Default
//!   `broker-v2-scaffold`.
//!
//! Future slices:
//! - SIGTERM / Ctrl+C graceful shutdown (drain in-flight handlers).
//! - Backend-pipe resolution + adopt forwarding.
//! - Single-instance lock (refuse start if another broker is bound).
//! - Refuse-privileged-run guard (port from v1).

use std::env;
use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use interprocess::local_socket::traits::Listener as _;
use interprocess::local_socket::ListenerOptions;
use prost::Message;
use running_process::broker::lifecycle::names_v2::v2_program_pipe;
use running_process::broker::lifecycle::privilege::refuse_privileged_run;
use running_process::broker::lifecycle::sid::user_sid_hash;
use running_process::broker::protocol::{
    hello_reply, read_frame, write_frame, ErrorCode, Hello, HelloReply, Negotiated, Refused,
    ENVELOPE_VERSION,
};
use running_process::broker::protocol_v2::ServiceDefinitionLoader;
use running_process::broker::server::service_def_loader::ServiceDefinitionError;

/// Default program name when `--program` is not passed. Matches the
/// slice-3c scaffold so existing integration tests keep working.
const DEFAULT_PROGRAM: &str = "broker-v2-scaffold";
const SCAFFOLD_PIPE_IDX: u32 = 0;

/// Maximum in-flight Hello handlers. Conservative cap; the OS thread
/// cap is the hard upper bound but we want backpressure before that.
const MAX_INFLIGHT_HANDLERS: usize = 256;

#[derive(Debug, Clone)]
struct CliOptions {
    no_bind: bool,
    once: bool,
    program: String,
}

fn parse_cli(args: &[String]) -> Result<CliOptions, String> {
    let mut opts = CliOptions {
        no_bind: false,
        once: false,
        program: DEFAULT_PROGRAM.to_owned(),
    };
    let mut i = 1; // skip argv[0]
    while i < args.len() {
        match args[i].as_str() {
            "--no-bind" => opts.no_bind = true,
            "--once" => opts.once = true,
            "--program" => {
                i += 1;
                if i >= args.len() {
                    return Err("--program requires a value".to_owned());
                }
                opts.program = args[i].clone();
            }
            "--help" | "-h" => {
                return Err(format!(
                    "running-process-broker-v2 {} — usage:\n  \
                     [--program <name>]  (default: {DEFAULT_PROGRAM})\n  \
                     [--once]            (accept one connection then exit)\n  \
                     [--no-bind]         (exit 0 immediately; for integration test)",
                    env!("CARGO_PKG_VERSION")
                ));
            }
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
        i += 1;
    }
    Ok(opts)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let opts = match parse_cli(&args) {
        Ok(o) => o,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    println!(
        "running-process-broker-v2 {} (slice 1 of running-process#532)",
        env!("CARGO_PKG_VERSION")
    );

    if opts.no_bind {
        println!("running-process-broker-v2 --no-bind: skipping listener bind");
        return ExitCode::SUCCESS;
    }

    // Slice 3 of #532: refuse to start as a privileged user. The
    // broker is a per-user daemon — running as root / LocalSystem
    // would bind the v2 pipe in a namespace other users can't reach
    // AND would create privileged sockets that downstream daemons
    // get adopted into. Mirrors v1's `running-process-broker-v1`
    // startup check exactly. The `RUNNING_PROCESS_ALLOW_PRIVILEGED`
    // env var is honored for isolated test environments that
    // intentionally exercise privileged startup behavior.
    if let Err(err) = refuse_privileged_run() {
        eprintln!(
            "running-process-broker-v2: refusing privileged startup: {err}. \
             Run as an unprivileged user, or set \
             RUNNING_PROCESS_ALLOW_PRIVILEGED=1 for isolated test environments only."
        );
        return ExitCode::from(77); // EX_NOPERM
    }

    let sid = match user_sid_hash() {
        Ok(s) => s,
        Err(err) => {
            eprintln!("running-process-broker-v2: user_sid_hash failed: {err}");
            return ExitCode::from(1);
        }
    };

    let pipe_name = match v2_program_pipe(&opts.program, &sid, SCAFFOLD_PIPE_IDX) {
        Ok(n) => n,
        Err(err) => {
            eprintln!("running-process-broker-v2: v2_program_pipe failed: {err}");
            return ExitCode::from(1);
        }
    };

    let socket_path = match resolve_socket_path(&pipe_name) {
        Ok(p) => p,
        Err(err) => {
            eprintln!("running-process-broker-v2: resolve_socket_path failed: {err}");
            return ExitCode::from(1);
        }
    };

    // Stale-file cleanup is Unix-only; on Windows the pipe namespace is
    // managed by the kernel and previous bindings vanish when the prior
    // process exited.
    #[cfg(unix)]
    {
        let path = std::path::Path::new(&socket_path);
        if let Some(parent) = path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                eprintln!(
                    "running-process-broker-v2: create_dir_all({}) failed: {err}",
                    parent.display()
                );
                return ExitCode::from(1);
            }
        }
        let _ = std::fs::remove_file(&socket_path);
    }

    let name = match wrap_socket_name(&socket_path) {
        Ok(n) => n,
        Err(err) => {
            eprintln!("running-process-broker-v2: wrap_socket_name failed: {err}");
            return ExitCode::from(1);
        }
    };

    let listener = match ListenerOptions::new().name(name).create_sync() {
        Ok(l) => l,
        Err(err) => {
            // Single-instance enforcement: a `WouldBlock` / `AddrInUse`
            // bind failure means another `running-process-broker-v2
            // --program {program}` is already running on this user's
            // socket. Surface a directly-actionable message instead of
            // a raw OS error string.
            if is_already_bound_error(&err) {
                eprintln!(
                    "running-process-broker-v2: another broker is already \
                     bound at {socket_path} (program={}). Refusing to \
                     start to avoid double-bind. Stop the other broker \
                     first, or pass `--program <other-name>` to bind a \
                     distinct namespace.",
                    opts.program,
                );
                return ExitCode::from(75); // EX_TEMPFAIL — supervisor can retry after the other broker exits
            }
            eprintln!("running-process-broker-v2: bind failed at {socket_path}: {err}");
            return ExitCode::from(1);
        }
    };

    println!(
        "running-process-broker-v2 bound at {socket_path} (program={}, mode={})",
        opts.program,
        if opts.once { "once" } else { "loop" }
    );
    if let Err(err) = std::io::stdout().flush() {
        eprintln!("running-process-broker-v2: stdout flush failed: {err}");
    }

    let loader = Arc::new(ServiceDefinitionLoader::default_root());
    let inflight = Arc::new(AtomicUsize::new(0));

    let exit_code = if opts.once {
        accept_one(&listener, Arc::clone(&loader))
    } else {
        accept_loop(&listener, Arc::clone(&loader), Arc::clone(&inflight))
    };

    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(&socket_path);
    }

    exit_code
}

/// Persistent accept loop. Spawns one handler thread per accepted
/// connection, bounded by `MAX_INFLIGHT_HANDLERS`. Returns `ExitCode`
/// only on terminal accept failure (the loop itself never returns
/// success — production exit is via SIGTERM, a follow-up slice).
fn accept_loop(
    listener: &interprocess::local_socket::Listener,
    loader: Arc<ServiceDefinitionLoader>,
    inflight: Arc<AtomicUsize>,
) -> ExitCode {
    loop {
        match listener.accept() {
            Ok(stream) => {
                // Backpressure: refuse to spawn if we're already at the cap.
                // The peer's blocking read on the Hello-reply socket will
                // see EOF when this branch closes the stream.
                let n = inflight.fetch_add(1, Ordering::SeqCst);
                if n >= MAX_INFLIGHT_HANDLERS {
                    inflight.fetch_sub(1, Ordering::SeqCst);
                    eprintln!(
                        "running-process-broker-v2: at MAX_INFLIGHT_HANDLERS ({MAX_INFLIGHT_HANDLERS}); dropping connection",
                    );
                    drop(stream);
                    continue;
                }
                let loader = Arc::clone(&loader);
                let inflight_handler = Arc::clone(&inflight);
                let spawn_result = thread::Builder::new()
                    .name("rpb-v2-handler".to_string())
                    .spawn(move || {
                        let mut s = stream;
                        let result = handle_hello(&mut s, &loader);
                        match result {
                            Ok(svc) => println!(
                                "running-process-broker-v2 Hello service={svc:?} negotiated",
                            ),
                            Err(err) => eprintln!(
                                "running-process-broker-v2 Hello handler failed: {err}"
                            ),
                        }
                        inflight_handler.fetch_sub(1, Ordering::SeqCst);
                    });
                if let Err(err) = spawn_result {
                    eprintln!(
                        "running-process-broker-v2: thread spawn failed: {err}; \
                         dropping connection"
                    );
                    // Decrement here since the spawned thread never ran.
                    inflight.fetch_sub(1, Ordering::SeqCst);
                }
            }
            Err(err) => {
                // accept() errors are typically fatal (listener died);
                // exit so a supervisor can restart us.
                eprintln!("running-process-broker-v2: accept failed: {err}");
                return ExitCode::from(1);
            }
        }
    }
}

/// One-shot accept (replaces the prior scaffold behavior; used by
/// `--once` for tests + by the slice-3c integration test).
fn accept_one(
    listener: &interprocess::local_socket::Listener,
    loader: Arc<ServiceDefinitionLoader>,
) -> ExitCode {
    match listener.accept() {
        Ok(mut stream) => {
            println!("running-process-broker-v2 peer connected (--once)");
            match handle_hello(&mut stream, &loader) {
                Ok(svc) => {
                    println!(
                        "running-process-broker-v2 Hello for service {svc:?} negotiated; exiting"
                    );
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("running-process-broker-v2: Hello handler failed: {err}");
                    ExitCode::from(1)
                }
            }
        }
        Err(err) => {
            eprintln!("running-process-broker-v2: accept failed: {err}");
            ExitCode::from(1)
        }
    }
}

/// Read a `Hello` frame, look up the registered service, and send
/// back either `Negotiated` (service found + version policy OK) or
/// `Refused` (unknown service or policy block).
///
/// Returns the service name on Negotiated, or the human-readable
/// refusal reason on Refused. Wire errors propagate as `Err`.
fn handle_hello<S: std::io::Read + std::io::Write>(
    stream: &mut S,
    loader: &ServiceDefinitionLoader,
) -> Result<String, String> {
    let bytes = read_frame(stream).map_err(|e| format!("read Hello frame: {e}"))?;
    let hello = Hello::decode(bytes.as_slice()).map_err(|e| format!("decode Hello: {e}"))?;

    let reply = build_hello_reply(&hello, loader);

    let mut body = Vec::with_capacity(reply.encoded_len());
    reply
        .encode(&mut body)
        .map_err(|e| format!("encode HelloReply: {e}"))?;
    write_frame(stream, &body).map_err(|e| format!("write HelloReply frame: {e}"))?;

    match reply.result {
        Some(hello_reply::Result::Negotiated(_)) => Ok(hello.service_name),
        Some(hello_reply::Result::Refused(r)) => Err(format!("refused: {}", r.reason)),
        None => Err("HelloReply missing result oneof".to_string()),
    }
}

/// Pure decision function — takes a Hello + a loader and returns the
/// HelloReply we should send. Split out from `handle_hello` so the
/// policy logic is unit-testable without standing up a real listener.
fn build_hello_reply(hello: &Hello, loader: &ServiceDefinitionLoader) -> HelloReply {
    // 1. Look up the service. Unknown service → ErrorServiceUnknown.
    let definition = match loader.load(&hello.service_name) {
        Ok(d) => d,
        Err(ServiceDefinitionError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            return refused_reply(
                hello,
                ErrorCode::ErrorServiceUnknown,
                "service definition was not found",
                0,
            );
        }
        Err(ServiceDefinitionError::InvalidName(_)) => {
            return refused_reply(
                hello,
                ErrorCode::ErrorServiceUnknown,
                "service name is invalid",
                0,
            );
        }
        Err(other) => {
            return refused_reply(
                hello,
                ErrorCode::ErrorServiceUnknown,
                format!("service definition could not be loaded: {other}"),
                0,
            );
        }
    };

    // 2. Version policy. min_version + version_allow_list per slice 22.
    if !definition.min_version.is_empty()
        && hello.wanted_version.as_str() < definition.min_version.as_str()
    {
        // Lexicographic for now (matches v1's pre-semver behaviour).
        // Real semver parsing is a follow-up; the contract is the
        // refusal reason + code, both already correct here.
        return refused_reply(
            hello,
            ErrorCode::ErrorVersionBlocked,
            format!(
                "wanted_version {:?} is below min_version {:?}",
                hello.wanted_version, definition.min_version
            ),
            0,
        );
    }
    if !definition.version_allow_list.is_empty()
        && !definition
            .version_allow_list
            .iter()
            .any(|v| v == &hello.wanted_version)
    {
        return refused_reply(
            hello,
            ErrorCode::ErrorVersionBlocked,
            format!(
                "wanted_version {:?} is not in version_allow_list",
                hello.wanted_version
            ),
            0,
        );
    }

    // 3. Happy path. Empty backend_pipe; real adopt-forwarding is a
    //    follow-up slice. The peer can still observe the Negotiated
    //    reply + the registered binary_path via subsequent control RPCs.
    HelloReply {
        result: Some(hello_reply::Result::Negotiated(Negotiated {
            negotiated_protocol: ENVELOPE_VERSION as u32,
            daemon_version: definition.min_version.clone(),
            backend_pipe: String::new(),
            warnings: Vec::new(),
            server_capabilities: 0,
            keepalive_interval_secs: 0,
            handle_passed_token: Vec::new(),
            connection_id: hello.connection_id,
        })),
    }
}

fn refused_reply(
    hello: &Hello,
    code: ErrorCode,
    reason: impl Into<String>,
    retry_after_ms: u64,
) -> HelloReply {
    HelloReply {
        result: Some(hello_reply::Result::Refused(Refused {
            reason: reason.into(),
            daemon_min_protocol: ENVELOPE_VERSION as u32,
            daemon_max_protocol: ENVELOPE_VERSION as u32,
            code: code as i32,
            details: std::collections::HashMap::new(),
            retry_after_ms,
        })),
    }
    .with_connection_id(hello.connection_id)
}

trait HelloReplyExt {
    fn with_connection_id(self, id: u64) -> Self;
}

impl HelloReplyExt for HelloReply {
    fn with_connection_id(mut self, id: u64) -> Self {
        if let Some(hello_reply::Result::Refused(_)) = &self.result {
            // Refused has no connection_id; nothing to thread.
        } else if let Some(hello_reply::Result::Negotiated(ref mut n)) = self.result {
            n.connection_id = id;
        }
        self
    }
}

/// Wrap a bare pipe name into the platform's local-socket path.
fn resolve_socket_path(bare_name: &str) -> Result<String, String> {
    #[cfg(windows)]
    {
        Ok(format!(r"\\.\pipe\{bare_name}"))
    }
    #[cfg(unix)]
    {
        let dir = unix_socket_dir();
        let leaf = if cfg!(target_os = "macos") {
            // macOS sun_path is 104 bytes; hash the bare name to fit.
            let mut hash = blake3::Hasher::new();
            hash.update(bare_name.as_bytes());
            let bytes = hash.finalize();
            let mut hex = String::with_capacity(16);
            for b in bytes.as_bytes().iter().take(8) {
                use std::fmt::Write as _;
                let _ = write!(hex, "{b:02x}");
            }
            format!("{hex}.sock")
        } else {
            format!("{bare_name}.sock")
        };
        Ok(dir.join(leaf).to_string_lossy().into_owned())
    }
}

#[cfg(unix)]
fn unix_socket_dir() -> std::path::PathBuf {
    use std::path::PathBuf;
    #[cfg(target_os = "macos")]
    {
        let uid = unsafe { libc::getuid() };
        let tmp = env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        tmp.join(format!(".rp-{uid}-broker-v2"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(d) = env::var_os("XDG_RUNTIME_DIR") {
            PathBuf::from(d).join("running-process").join("broker-v2")
        } else {
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/running-process-{uid}/broker-v2"))
        }
    }
}

/// Classify a [`ListenerOptions::create_sync`] error as
/// "another broker is already bound" vs any other bind failure.
///
/// Single-instance enforcement is delegated to the OS: a `WouldBlock`
/// or `AddrInUse` from the kernel's pipe namespace is the canonical
/// "another listener already owns this name" signal. The slice 1
/// scaffold treated every bind failure equivalently; this slice
/// separates the user-actionable case (another broker running)
/// from environment failures (permission denied, parent dir
/// missing, etc.) so supervisors can react appropriately
/// (retry-after-exit vs hard-fail).
fn is_already_bound_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::AddrInUse | std::io::ErrorKind::WouldBlock,
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use running_process::broker::protocol_v2::ServiceDefinitionBuilder;
    use tempfile::tempdir;

    fn make_hello(service: &str, wanted: &str) -> Hello {
        Hello {
            client_min_protocol: ENVELOPE_VERSION as u32,
            client_max_protocol: ENVELOPE_VERSION as u32,
            service_name: service.to_string(),
            wanted_version: wanted.to_string(),
            client_version: "test".to_string(),
            client_capabilities: 0,
            auth_token: Vec::new(),
            request_id: "test".to_string(),
            connection_id: 42,
            peer_pid: 1234,
            client_lib_name: "test".to_string(),
            client_lib_version: "test".to_string(),
            peer_attestation_nonce: Vec::new(),
            capability_token: Vec::new(),
            client_keepalive_secs: 0,
        }
    }

    #[test]
    fn parse_cli_defaults() {
        let args = vec!["bin".to_owned()];
        let opts = parse_cli(&args).unwrap();
        assert!(!opts.no_bind);
        assert!(!opts.once);
        assert_eq!(opts.program, DEFAULT_PROGRAM);
    }

    #[test]
    fn parse_cli_program_arg() {
        let args = vec!["bin".to_owned(), "--program".to_owned(), "zccache".to_owned()];
        let opts = parse_cli(&args).unwrap();
        assert_eq!(opts.program, "zccache");
    }

    #[test]
    fn parse_cli_once_flag() {
        let args = vec!["bin".to_owned(), "--once".to_owned()];
        let opts = parse_cli(&args).unwrap();
        assert!(opts.once);
    }

    #[test]
    fn parse_cli_program_missing_value_errs() {
        let args = vec!["bin".to_owned(), "--program".to_owned()];
        assert!(parse_cli(&args).is_err());
    }

    #[test]
    fn parse_cli_unknown_arg_errs() {
        let args = vec!["bin".to_owned(), "--bogus".to_owned()];
        assert!(parse_cli(&args).is_err());
    }

    #[test]
    fn build_hello_reply_refuses_unknown_service() {
        let dir = tempdir().unwrap();
        let loader = ServiceDefinitionLoader::new(dir.path());
        let hello = make_hello("nosuch", "1.0.0");
        let reply = build_hello_reply(&hello, &loader);
        match reply.result {
            Some(hello_reply::Result::Refused(r)) => {
                assert_eq!(r.code, ErrorCode::ErrorServiceUnknown as i32);
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn build_hello_reply_negotiates_registered_service() {
        let dir = tempdir().unwrap();
        ServiceDefinitionBuilder::shared_broker("zccache", "/usr/bin/zccache-daemon")
            .install_in(dir.path())
            .unwrap();
        let loader = ServiceDefinitionLoader::new(dir.path());
        let hello = make_hello("zccache", "1.0.0");
        let reply = build_hello_reply(&hello, &loader);
        match reply.result {
            Some(hello_reply::Result::Negotiated(n)) => {
                assert_eq!(n.connection_id, 42);
                assert!(n.backend_pipe.is_empty(), "adopt forwarding is follow-up");
            }
            other => panic!("expected Negotiated, got {other:?}"),
        }
    }

    #[test]
    fn build_hello_reply_blocks_below_min_version() {
        let dir = tempdir().unwrap();
        ServiceDefinitionBuilder::shared_broker("zccache", "/usr/bin/zccache-daemon")
            .min_version("2.0.0")
            .install_in(dir.path())
            .unwrap();
        let loader = ServiceDefinitionLoader::new(dir.path());
        let hello = make_hello("zccache", "1.0.0");
        let reply = build_hello_reply(&hello, &loader);
        match reply.result {
            Some(hello_reply::Result::Refused(r)) => {
                assert_eq!(r.code, ErrorCode::ErrorVersionBlocked as i32);
                assert!(r.reason.contains("min_version"), "got: {}", r.reason);
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn build_hello_reply_blocks_outside_version_allow_list() {
        let dir = tempdir().unwrap();
        ServiceDefinitionBuilder::shared_broker("zccache", "/usr/bin/zccache-daemon")
            .version_allow_list(["1.0.0", "1.1.0"])
            .install_in(dir.path())
            .unwrap();
        let loader = ServiceDefinitionLoader::new(dir.path());
        let hello = make_hello("zccache", "1.2.0");
        let reply = build_hello_reply(&hello, &loader);
        match reply.result {
            Some(hello_reply::Result::Refused(r)) => {
                assert_eq!(r.code, ErrorCode::ErrorVersionBlocked as i32);
                assert!(
                    r.reason.contains("allow_list"),
                    "got: {}",
                    r.reason
                );
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn is_already_bound_error_classifies_addr_in_use() {
        let err = std::io::Error::new(std::io::ErrorKind::AddrInUse, "in use");
        assert!(is_already_bound_error(&err));
    }

    #[test]
    fn is_already_bound_error_classifies_would_block() {
        let err = std::io::Error::new(std::io::ErrorKind::WouldBlock, "would block");
        assert!(is_already_bound_error(&err));
    }

    #[test]
    fn is_already_bound_error_does_not_misclassify_permission_denied() {
        let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        assert!(!is_already_bound_error(&err));
    }

    #[test]
    fn is_already_bound_error_does_not_misclassify_not_found() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        assert!(!is_already_bound_error(&err));
    }

    #[test]
    fn build_hello_reply_allows_version_in_allow_list() {
        let dir = tempdir().unwrap();
        ServiceDefinitionBuilder::shared_broker("zccache", "/usr/bin/zccache-daemon")
            .version_allow_list(["1.0.0", "1.1.0"])
            .install_in(dir.path())
            .unwrap();
        let loader = ServiceDefinitionLoader::new(dir.path());
        let hello = make_hello("zccache", "1.1.0");
        let reply = build_hello_reply(&hello, &loader);
        assert!(matches!(
            reply.result,
            Some(hello_reply::Result::Negotiated(_))
        ));
    }
}
