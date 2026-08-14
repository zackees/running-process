//! Tests for the compile-session handler (soldr#2365). Drive
//! [`run_compile_session`] over the SESSION-lane [`SessionFrameCodec`](super::SessionFrameCodec)
//! wire and assert the proxied bytes match a direct-execution oracle:
//! in-process over a tokio `duplex()` (fast, deterministic), and — per the #2386
//! ruling's "real sockets" mandate — over an **actual Unix-domain socket**
//! (accept + connect), which exercises partial-frame reads a `duplex()` hides.
//! Run in CI by a scoped `--features daemon -E test(compile_session)` nextest
//! step.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};

use super::{run_compile_session, serve_session, session_framed};
use crate::broker::protocol_v2::{session_frame, SessionFrame, SessionStart};
use crate::containment::ContainedProcessGroup;

/// The fixture binary's path as the string a `SessionStart.program` carries.
fn fixture_program() -> String {
    let exe = std::env::current_exe().expect("test executable path");
    let dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    dir.join(format!(
        "testbin-stdio-scripted{}",
        std::env::consts::EXE_SUFFIX
    ))
    .to_string_lossy()
    .into_owned()
}

fn fixture() -> Command {
    let exe = std::env::current_exe().expect("test executable path");
    let dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    let path = dir.join(format!(
        "testbin-stdio-scripted{}",
        std::env::consts::EXE_SUFFIX
    ));
    assert!(
        path.is_file(),
        "test fixture is missing at {} — run `soldr cargo build -p testbins` first",
        path.display()
    );
    Command::new(path)
}

fn frame(kind: session_frame::Kind) -> SessionFrame {
    SessionFrame { kind: Some(kind) }
}

#[derive(Debug, PartialEq, Eq)]
struct Reassembled {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: i32,
}

/// Direct execution (the oracle/golden).
fn run_direct(directives: &[&str], stdin: &[u8]) -> Reassembled {
    let mut child = fixture()
        .args(directives)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oracle");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    Reassembled {
        stdout: out.stdout,
        stderr: out.stderr,
        code: out.status.code().unwrap_or(-1),
    }
}

/// Drive the handler over the daemon transport with a codec-speaking client.
async fn run_over_daemon_transport(directives: &[&str], stdin: &[u8]) -> Reassembled {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = session_framed(server_io);
    let mut client = session_framed(client_io);

    let mut cmd = fixture();
    cmd.args(directives);
    let group = Arc::new(ContainedProcessGroup::new().expect("contained group"));

    let handler = tokio::spawn(run_compile_session(server, cmd, group));

    if !stdin.is_empty() {
        client
            .send(frame(session_frame::Kind::Stdin(stdin.to_vec())))
            .await
            .unwrap();
    }
    client
        .send(frame(session_frame::Kind::StdinEof(true)))
        .await
        .unwrap();

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut code = None;
    while let Some(Ok(sf)) = client.next().await {
        match sf.kind {
            Some(session_frame::Kind::Stdout(b)) => stdout.extend_from_slice(&b),
            Some(session_frame::Kind::Stderr(b)) => stderr.extend_from_slice(&b),
            Some(session_frame::Kind::Exit(e)) => {
                code = Some(e.code);
                break;
            }
            _ => panic!("unexpected inbound-only frame on the outbound lane"),
        }
    }

    let exit = handler.await.unwrap().expect("handler reaps child");
    assert_eq!(code, Some(exit.code), "client-visible exit matches handler");
    Reassembled {
        stdout,
        stderr,
        code: exit.code,
    }
}

#[tokio::test]
async fn compile_session_matches_oracle_over_daemon_transport() {
    let script = &["out:ARTIFACT", "err:DIAGNOSTIC", "out:MORE", "exit:5"];
    assert_eq!(
        run_over_daemon_transport(script, b"").await,
        run_direct(script, b"")
    );
}

#[tokio::test]
async fn compile_session_is_byte_transparent_for_raw_non_utf8() {
    let script = &["outhex:00ff80", "errhex:8081ff"];
    let got = run_over_daemon_transport(script, b"").await;
    assert_eq!(got.stdout, vec![0x00, 0xff, 0x80]);
    assert_eq!(got.stderr, vec![0x80, 0x81, 0xff]);
    assert_eq!(got, run_direct(script, b""));
}

#[tokio::test]
async fn compile_session_delivers_full_byte_range_stdin() {
    let payload: Vec<u8> = (0u8..=255).collect();
    let got = run_over_daemon_transport(&["echo"], &payload).await;
    assert_eq!(got.stdout, payload);
    assert_eq!(got, run_direct(&["echo"], &payload));
}

/// The daemon's half of the Phase-3 relay vertical, over an **actual Unix-domain
/// socket** (#2386 ruling: real sockets, not oracle-only `duplex()`). A listener
/// accepts one connection and runs the session; a client dials it over the same
/// real socket and speaks the SESSION-lane wire. Unlike `duplex()`, the kernel
/// may split a `[1][len][Frame]` mid-header, so this exercises the
/// buffer-until-complete partial-frame path in [`SessionFrameCodec`](super::SessionFrameCodec).
/// Unix-first per invariant 7; the Windows named-pipe equivalent lands later.
#[cfg(unix)]
#[tokio::test]
async fn compile_session_matches_oracle_over_real_unix_socket() {
    use tokio::net::{UnixListener, UnixStream};

    // A unique, short socket path (sun_path is length-limited). nextest runs
    // each test in its own process, so the pid uniquely names this socket.
    let sock = std::env::temp_dir().join(format!("rp-sess-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).expect("bind unix listener");

    let script: &[&str] = &["out:ARTIFACT", "err:DIAGNOSTIC", "out:MORE", "exit:5"];
    let mut cmd = fixture();
    cmd.args(script);
    let group = Arc::new(ContainedProcessGroup::new().expect("contained group"));

    // Daemon side: accept one real connection and run the session over it.
    let server = tokio::spawn(async move {
        let (io, _addr) = listener.accept().await.expect("accept");
        run_compile_session(session_framed(io), cmd, group).await
    });

    // Client side: dial the same real socket and speak the SESSION wire.
    let client_io = UnixStream::connect(&sock)
        .await
        .expect("connect unix socket");
    let mut client = session_framed(client_io);
    client
        .send(frame(session_frame::Kind::StdinEof(true)))
        .await
        .expect("send stdin eof");

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut code = None;
    while let Some(Ok(sf)) = client.next().await {
        match sf.kind {
            Some(session_frame::Kind::Stdout(b)) => stdout.extend_from_slice(&b),
            Some(session_frame::Kind::Stderr(b)) => stderr.extend_from_slice(&b),
            Some(session_frame::Kind::Exit(e)) => {
                code = Some(e.code);
                break;
            }
            _ => panic!("unexpected inbound-only frame on the outbound lane"),
        }
    }

    let exit = server
        .await
        .expect("server task")
        .expect("handler reaps child");
    let _ = std::fs::remove_file(&sock);

    let oracle = run_direct(script, b"");
    assert_eq!(code, Some(exit.code), "client-visible exit matches handler");
    assert_eq!(
        stdout, oracle.stdout,
        "stdout byte-exact across a real socket"
    );
    assert_eq!(
        stderr, oracle.stderr,
        "stderr byte-exact across a real socket"
    );
    assert_eq!(
        exit.code, oracle.code,
        "exit code fidelity across a real socket"
    );
}

/// The full Phase-3 relay data path (#2386 ruling): one compile session proxied
/// **client → broker → daemon across two real Unix sockets**. The "broker" is a
/// minimal full-proxy — it accepts the client, dials the daemon, and relays
/// bytes both ways with `copy_bidirectional` (this is the model that *replaces*
/// the legacy handle-passing handoff). The daemon serves the session on its own
/// real endpoint. Byte-exact stdout/stderr and exit fidelity must survive both
/// hops. Proves the SESSION wire tolerates a relay and two socket boundaries;
/// the production broker adds Hello/routing/backpressure on top. Unix-first.
#[cfg(unix)]
#[tokio::test]
async fn compile_session_matches_oracle_across_broker_relay() {
    use tokio::net::{UnixListener, UnixStream};

    let pid = std::process::id();
    let daemon_sock = std::env::temp_dir().join(format!("rp-relay-d-{pid}.sock"));
    let broker_sock = std::env::temp_dir().join(format!("rp-relay-b-{pid}.sock"));
    let _ = std::fs::remove_file(&daemon_sock);
    let _ = std::fs::remove_file(&broker_sock);
    let daemon_listener = UnixListener::bind(&daemon_sock).expect("bind daemon listener");
    let broker_listener = UnixListener::bind(&broker_sock).expect("bind broker listener");

    let script: &[&str] = &["out:ARTIFACT", "err:DIAGNOSTIC", "out:MORE", "exit:5"];
    let mut cmd = fixture();
    cmd.args(script);
    let group = Arc::new(ContainedProcessGroup::new().expect("contained group"));

    // Daemon: accept its real connection and serve the session.
    let daemon = tokio::spawn(async move {
        let (io, _addr) = daemon_listener.accept().await.expect("daemon accept");
        run_compile_session(session_framed(io), cmd, group).await
    });

    // Broker: accept the client, dial the daemon, relay bytes both ways.
    let daemon_sock_for_broker = daemon_sock.clone();
    let broker = tokio::spawn(async move {
        let (mut client_conn, _addr) = broker_listener.accept().await.expect("broker accept");
        let mut daemon_conn = UnixStream::connect(&daemon_sock_for_broker)
            .await
            .expect("broker dials daemon");
        // Full-proxy: every byte flows through the broker, both directions.
        let _ = tokio::io::copy_bidirectional(&mut client_conn, &mut daemon_conn).await;
    });

    // Client: dial the broker (never the daemon) and speak the SESSION wire.
    let client_io = UnixStream::connect(&broker_sock)
        .await
        .expect("client dials broker");
    let mut client = session_framed(client_io);
    client
        .send(frame(session_frame::Kind::StdinEof(true)))
        .await
        .expect("send stdin eof");

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut code = None;
    while let Some(Ok(sf)) = client.next().await {
        match sf.kind {
            Some(session_frame::Kind::Stdout(b)) => stdout.extend_from_slice(&b),
            Some(session_frame::Kind::Stderr(b)) => stderr.extend_from_slice(&b),
            Some(session_frame::Kind::Exit(e)) => {
                code = Some(e.code);
                break;
            }
            _ => panic!("unexpected inbound-only frame on the outbound lane"),
        }
    }
    // Close the client leg so the relay's client→daemon half reaches EOF.
    drop(client);

    let exit = daemon
        .await
        .expect("daemon task")
        .expect("handler reaps child");
    let _ = broker.await;
    let _ = std::fs::remove_file(&daemon_sock);
    let _ = std::fs::remove_file(&broker_sock);

    let oracle = run_direct(script, b"");
    assert_eq!(code, Some(exit.code), "client-visible exit matches handler");
    assert_eq!(stdout, oracle.stdout, "stdout byte-exact across the relay");
    assert_eq!(stderr, oracle.stderr, "stderr byte-exact across the relay");
    assert_eq!(
        exit.code, oracle.code,
        "exit code fidelity across the relay"
    );
}

/// Kill-matrix cell (#2361/#2360 "core of the design"): **broker death mid-session
/// is detected by the client within a bounded latency** — no hang, no 30s budget.
/// A session is established end-to-end over the relay (a stdin byte echoed back
/// proves the path is live), then the broker relay is aborted mid-flight; the
/// client's stream must reach EOF/error promptly. The bound is asserted with a
/// number. Unix-first.
#[cfg(unix)]
#[tokio::test]
async fn broker_kill_mid_session_detected_by_client_within_bound() {
    use std::time::{Duration, Instant};
    use tokio::net::{UnixListener, UnixStream};

    let pid = std::process::id();
    let daemon_sock = std::env::temp_dir().join(format!("rp-kill-d-{pid}.sock"));
    let broker_sock = std::env::temp_dir().join(format!("rp-kill-b-{pid}.sock"));
    let _ = std::fs::remove_file(&daemon_sock);
    let _ = std::fs::remove_file(&broker_sock);
    let daemon_listener = UnixListener::bind(&daemon_sock).expect("bind daemon listener");
    let broker_listener = UnixListener::bind(&broker_sock).expect("bind broker listener");

    // A long-lived session that also emits output UNPROMPTED so we can prove the
    // path is live before killing: `out:ping` writes immediately, then `echo`
    // reads stdin until EOF — with no StdinEof the child stays alive, so the
    // session is genuinely mid-flight when we kill. (`echo` alone would block
    // waiting for stdin EOF before producing any output.)
    let mut cmd = fixture();
    cmd.args(["out:ping", "echo"]);
    let group = Arc::new(ContainedProcessGroup::new().expect("contained group"));

    let daemon = tokio::spawn(async move {
        let (io, _addr) = daemon_listener.accept().await.expect("daemon accept");
        run_compile_session(session_framed(io), cmd, group).await
    });

    let daemon_sock_for_broker = daemon_sock.clone();
    let broker = tokio::spawn(async move {
        let (mut client_conn, _addr) = broker_listener.accept().await.expect("broker accept");
        let mut daemon_conn = UnixStream::connect(&daemon_sock_for_broker)
            .await
            .expect("broker dials daemon");
        let _ = tokio::io::copy_bidirectional(&mut client_conn, &mut daemon_conn).await;
    });

    let client_io = UnixStream::connect(&broker_sock)
        .await
        .expect("client dials broker");
    let mut client = session_framed(client_io);

    // Prove the path is live end-to-end: the child's unprompted `out:ping`
    // reaches the client across both relay legs.
    let live = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .expect("first output within 5s")
        .expect("a frame")
        .expect("decode ok");
    assert_eq!(
        live.kind,
        Some(session_frame::Kind::Stdout(b"ping".to_vec())),
        "session is live over the relay before the kill"
    );

    // Kill the broker mid-session.
    let t0 = Instant::now();
    broker.abort();

    // The client must detect the disconnect (EOF/error), not hang.
    let detected = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match client.next().await {
                None | Some(Err(_)) => break,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await;
    let latency = t0.elapsed();

    assert!(
        detected.is_ok(),
        "client hung after broker death instead of detecting it"
    );
    assert!(
        latency < Duration::from_secs(1),
        "broker-death detection latency {latency:?} exceeds the 1s bound"
    );

    // The daemon side sees its connection drop and tears the session down (its
    // inbound stream EOFs, closing the child's stdin so `echo` exits) — no hang.
    let _ = tokio::time::timeout(Duration::from_secs(5), daemon)
        .await
        .expect("daemon session terminates after its connection drops");

    let _ = std::fs::remove_file(&daemon_sock);
    let _ = std::fs::remove_file(&broker_sock);
}

/// Kill-matrix cell: **client death mid-session is detected by the daemon within
/// a bounded latency**, which cancels the unit — the daemon does not leak the
/// session or the child. A live session is established over the relay, then the
/// client is dropped mid-flight; the client's EOF propagates through the broker
/// relay (copy_bidirectional shuts the daemon's read half), the daemon's inbound
/// stream EOFs, its handler closes the child's stdin (`echo` exits), and
/// `run_compile_session` returns — all within an asserted numeric bound. The
/// broker relay also completes cleanly. Unix-first.
#[cfg(unix)]
#[tokio::test]
async fn client_kill_mid_session_torn_down_by_daemon_within_bound() {
    use std::time::{Duration, Instant};
    use tokio::net::{UnixListener, UnixStream};

    let pid = std::process::id();
    let daemon_sock = std::env::temp_dir().join(format!("rp-ckill-d-{pid}.sock"));
    let broker_sock = std::env::temp_dir().join(format!("rp-ckill-b-{pid}.sock"));
    let _ = std::fs::remove_file(&daemon_sock);
    let _ = std::fs::remove_file(&broker_sock);
    let daemon_listener = UnixListener::bind(&daemon_sock).expect("bind daemon listener");
    let broker_listener = UnixListener::bind(&broker_sock).expect("bind broker listener");

    let mut cmd = fixture();
    cmd.args(["out:ping", "echo"]);
    let group = Arc::new(ContainedProcessGroup::new().expect("contained group"));

    let daemon = tokio::spawn(async move {
        let (io, _addr) = daemon_listener.accept().await.expect("daemon accept");
        run_compile_session(session_framed(io), cmd, group).await
    });

    let daemon_sock_for_broker = daemon_sock.clone();
    let broker = tokio::spawn(async move {
        let (mut client_conn, _addr) = broker_listener.accept().await.expect("broker accept");
        let mut daemon_conn = UnixStream::connect(&daemon_sock_for_broker)
            .await
            .expect("broker dials daemon");
        let _ = tokio::io::copy_bidirectional(&mut client_conn, &mut daemon_conn).await;
    });

    let client_io = UnixStream::connect(&broker_sock)
        .await
        .expect("client dials broker");
    let mut client = session_framed(client_io);

    // Session is live end-to-end (unprompted out:ping reaches the client).
    let live = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .expect("first output within 5s")
        .expect("a frame")
        .expect("decode ok");
    assert_eq!(
        live.kind,
        Some(session_frame::Kind::Stdout(b"ping".to_vec())),
        "session is live over the relay before the kill"
    );

    // Kill the client mid-session.
    let t0 = Instant::now();
    drop(client);

    // The daemon must detect the disconnect and tear the session down (return),
    // not leak it. `echo` exits once its stdin closes, so the session ends.
    let outcome = tokio::time::timeout(Duration::from_secs(2), daemon).await;
    let latency = t0.elapsed();
    assert!(
        outcome.is_ok(),
        "daemon leaked the session after client death instead of tearing it down"
    );
    outcome
        .unwrap()
        .expect("daemon task")
        .expect("daemon reaps the child cleanly");
    assert!(
        latency < Duration::from_secs(1),
        "client-death teardown latency {latency:?} exceeds the 1s bound"
    );

    // The broker relay completes once both legs close — no lingering relay.
    let _ = tokio::time::timeout(Duration::from_secs(5), broker)
        .await
        .expect("broker relay completes after both legs close");

    let _ = std::fs::remove_file(&daemon_sock);
    let _ = std::fs::remove_file(&broker_sock);
}

/// Kill-matrix cell: **daemon death mid-session is detected by the client within
/// a bounded latency**, through the broker relay. The daemon's session is
/// aborted (its socket closes, as it would on daemon-process death); the broker
/// relay propagates that EOF to the client, whose stream ends promptly — asserted
/// with a number (< 1s). The *reaping* half of daemon-death (the contained child
/// is killed, no orphan rustc) is an OS-level guarantee proven separately by
/// `tests/async_owner_death_test.rs` (kill-when-owner-dies) and
/// `tests/daemon_tree_kill_test.rs` (whole-tree, no orphan grandchildren); this
/// cell covers the client-observable cancellation the relay must deliver.
/// Unix-first.
#[cfg(unix)]
#[tokio::test]
async fn daemon_kill_mid_session_detected_by_client_within_bound() {
    use std::time::{Duration, Instant};
    use tokio::net::{UnixListener, UnixStream};

    let pid = std::process::id();
    let daemon_sock = std::env::temp_dir().join(format!("rp-dkill-d-{pid}.sock"));
    let broker_sock = std::env::temp_dir().join(format!("rp-dkill-b-{pid}.sock"));
    let _ = std::fs::remove_file(&daemon_sock);
    let _ = std::fs::remove_file(&broker_sock);
    let daemon_listener = UnixListener::bind(&daemon_sock).expect("bind daemon listener");
    let broker_listener = UnixListener::bind(&broker_sock).expect("bind broker listener");

    let mut cmd = fixture();
    cmd.args(["out:ping", "echo"]);
    let group = Arc::new(ContainedProcessGroup::new().expect("contained group"));

    // Keep the daemon handle so we can abort it (simulating daemon death).
    let daemon = tokio::spawn(async move {
        let (io, _addr) = daemon_listener.accept().await.expect("daemon accept");
        run_compile_session(session_framed(io), cmd, group).await
    });

    let daemon_sock_for_broker = daemon_sock.clone();
    let broker = tokio::spawn(async move {
        let (mut client_conn, _addr) = broker_listener.accept().await.expect("broker accept");
        let mut daemon_conn = UnixStream::connect(&daemon_sock_for_broker)
            .await
            .expect("broker dials daemon");
        let _ = tokio::io::copy_bidirectional(&mut client_conn, &mut daemon_conn).await;
    });

    let client_io = UnixStream::connect(&broker_sock)
        .await
        .expect("client dials broker");
    let mut client = session_framed(client_io);

    let live = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .expect("first output within 5s")
        .expect("a frame")
        .expect("decode ok");
    assert_eq!(
        live.kind,
        Some(session_frame::Kind::Stdout(b"ping".to_vec())),
        "session is live over the relay before the kill"
    );

    // Kill the daemon mid-session; its socket closes and the relay carries the
    // EOF to the client.
    let t0 = Instant::now();
    daemon.abort();

    let detected = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match client.next().await {
                None | Some(Err(_)) => break,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await;
    let latency = t0.elapsed();

    assert!(
        detected.is_ok(),
        "client hung after daemon death instead of detecting it"
    );
    assert!(
        latency < Duration::from_secs(1),
        "daemon-death detection latency {latency:?} exceeds the 1s bound"
    );

    // The client is still open, so the relay's client→daemon half hasn't hit
    // EOF; just tear the relay task down (clean relay completion is covered by
    // the relay test) rather than waiting on it.
    broker.abort();
    let _ = std::fs::remove_file(&daemon_sock);
    let _ = std::fs::remove_file(&broker_sock);
}

/// The real daemon serve entry (#2365 command carriage): the command is carried
/// on the wire in the opening `SessionStart` frame, not passed by the caller.
/// `serve_session` reads it, builds the contained child, and proxies the rest of
/// the session — byte-exact vs the direct oracle.
#[tokio::test]
async fn serve_session_runs_command_from_start_frame() {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = session_framed(server_io);
    let mut client = session_framed(client_io);
    let group = Arc::new(ContainedProcessGroup::new().expect("contained group"));

    let handler = tokio::spawn(serve_session(server, group));

    let script = ["out:ARTIFACT", "err:DIAGNOSTIC", "out:MORE", "exit:5"];
    client
        .send(frame(session_frame::Kind::Start(SessionStart {
            program: fixture_program(),
            args: script.iter().map(|s| s.to_string()).collect(),
            cwd: String::new(),
            env: Vec::new(),
            clear_inherited_env: false,
            environment_policy: 0,
        })))
        .await
        .expect("send start");
    client
        .send(frame(session_frame::Kind::StdinEof(true)))
        .await
        .expect("send stdin eof");

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut code = None;
    while let Some(Ok(sf)) = client.next().await {
        match sf.kind {
            Some(session_frame::Kind::Stdout(b)) => stdout.extend_from_slice(&b),
            Some(session_frame::Kind::Stderr(b)) => stderr.extend_from_slice(&b),
            Some(session_frame::Kind::Exit(e)) => {
                code = Some(e.code);
                break;
            }
            _ => panic!("unexpected inbound-only frame on the outbound lane"),
        }
    }

    let exit = handler
        .await
        .expect("handler task")
        .expect("serve reaps child");
    let oracle = run_direct(&script, b"");
    assert_eq!(code, Some(exit.code), "client-visible exit matches handler");
    assert_eq!(stdout, oracle.stdout, "stdout byte-exact");
    assert_eq!(stderr, oracle.stderr, "stderr byte-exact");
    assert_eq!(exit.code, oracle.code, "exit code fidelity");
}

/// A remote SESSION child is owned by the client context, not by the
/// long-lived daemon's ambient environment. The client snapshot must replace
/// that ambient base while still reaching the child intact.
#[tokio::test]
async fn serve_session_replaces_daemon_environment_with_client_snapshot() {
    const DAEMON_ONLY: &str = "RUNNING_PROCESS_TEST_DAEMON_ONLY_ENV";
    const CLIENT_ONLY: &str = "RUNNING_PROCESS_TEST_CLIENT_ONLY_ENV";
    let previous_daemon_value = std::env::var_os(DAEMON_ONLY);
    std::env::set_var(DAEMON_ONLY, "must-not-leak");

    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = session_framed(server_io);
    let mut client = session_framed(client_io);
    let group = Arc::new(ContainedProcessGroup::new().expect("contained group"));
    let handler = tokio::spawn(serve_session(server, group));

    #[cfg(windows)]
    let (program, args) = (
        "cmd.exe".to_owned(),
        vec!["/D".to_owned(), "/C".to_owned(), "set".to_owned()],
    );
    #[cfg(not(windows))]
    let (program, args) = ("/usr/bin/env".to_owned(), Vec::new());

    client
        .send(frame(session_frame::Kind::Start(SessionStart {
            program,
            args,
            cwd: String::new(),
            env: vec![crate::broker::protocol_v2::SessionEnvVar {
                key: CLIENT_ONLY.to_owned(),
                value: "forwarded".to_owned(),
            }],
            clear_inherited_env: true,
            environment_policy: 3,
        })))
        .await
        .expect("send start");
    client
        .send(frame(session_frame::Kind::StdinEof(true)))
        .await
        .expect("send stdin eof");

    let mut stdout = Vec::new();
    while let Some(Ok(sf)) = client.next().await {
        match sf.kind {
            Some(session_frame::Kind::Stdout(bytes)) => stdout.extend_from_slice(&bytes),
            Some(session_frame::Kind::Exit(_)) => break,
            _ => {}
        }
    }
    handler.await.expect("handler task").expect("serve session");
    match previous_daemon_value {
        Some(value) => std::env::set_var(DAEMON_ONLY, value),
        None => std::env::remove_var(DAEMON_ONLY),
    }

    let output = String::from_utf8_lossy(&stdout);
    assert!(
        output.contains(&format!("{CLIENT_ONLY}=forwarded")),
        "client environment did not reach child"
    );
    assert!(
        !output.contains(DAEMON_ONLY),
        "daemon-only environment leaked into SESSION child"
    );
}

/// A session that does not open with `SessionStart` is a protocol error, not a
/// hang or a silent default.
#[tokio::test]
async fn serve_session_rejects_missing_start_frame() {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = session_framed(server_io);
    let mut client = session_framed(client_io);
    let group = Arc::new(ContainedProcessGroup::new().expect("contained group"));

    let handler = tokio::spawn(serve_session(server, group));

    // Open with stdin instead of the mandatory Start.
    client
        .send(frame(session_frame::Kind::Stdin(b"oops".to_vec())))
        .await
        .expect("send stdin");
    drop(client);

    let result = handler.await.expect("handler task");
    assert!(
        result.is_err(),
        "serve_session must reject a session that skips SessionStart"
    );
}

#[tokio::test]
async fn serve_session_rejects_unknown_environment_policy() {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = session_framed(server_io);
    let mut client = session_framed(client_io);
    let group = Arc::new(ContainedProcessGroup::new().expect("contained group"));
    let handler = tokio::spawn(serve_session(server, group));

    client
        .send(frame(session_frame::Kind::Start(SessionStart {
            program: fixture_program(),
            args: Vec::new(),
            cwd: String::new(),
            env: Vec::new(),
            clear_inherited_env: false,
            environment_policy: 99,
        })))
        .await
        .expect("send start");
    drop(client);

    let error = handler
        .await
        .expect("handler task")
        .expect_err("unknown policy must fail closed");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}
