use super::*;

const INHERITED_LISTENER_CHILD_ENV: &str = "RUNNING_PROCESS_TEST_INHERITED_LISTENER_CHILD";
const CHILD_EXPECTS_CLOSED: &str = "closed";
const CHILD_EXPECTS_INHERITED: &str = "inherited";
const CHILD_TEST_NAME: &str = "broker::broker_owned_bind::tests::inherited_listener_daemon_child";

/// The launcher binds by default, and a falsy value opts out.
///
/// The escape hatch used to open for `0` alone; it now opens for every falsy
/// spelling, because a user who writes `=false` is plainly asking for the
/// fallback and silently keeping the default is the wrong way to be wrong.
#[test]
fn the_launcher_binds_by_default_and_a_falsy_value_opts_out() {
    let name = LAUNCHER_OPT_IN_ENV;
    assert!(with_launcher_opt_in(None, launcher_opt_in), "unset binds");
    for still_binds in ["1", "true", "yes", "on", "anything-else"] {
        assert!(
            with_launcher_opt_in(Some(still_binds), launcher_opt_in),
            "{still_binds:?} must leave broker-owned bind in place ({name})"
        );
    }
    for opts_out in ["0", "false", "no", "off", ""] {
        assert!(
            !with_launcher_opt_in(Some(opts_out), launcher_opt_in),
            "{opts_out:?} must fall back to spawn-then-probe ({name})"
        );
    }
}

fn with_launcher_opt_in<T>(value: Option<&str>, body: impl FnOnce() -> T) -> T {
    let previous = std::env::var_os(LAUNCHER_OPT_IN_ENV);
    match value {
        Some(value) => std::env::set_var(LAUNCHER_OPT_IN_ENV, value),
        None => std::env::remove_var(LAUNCHER_OPT_IN_ENV),
    }
    let outcome = body();
    match previous {
        Some(previous) => std::env::set_var(LAUNCHER_OPT_IN_ENV, previous),
        None => std::env::remove_var(LAUNCHER_OPT_IN_ENV),
    }
    outcome
}

#[test]
fn environment_contract_is_project_namespaced() {
    assert!(LAUNCHER_OPT_IN_ENV.starts_with("RUNNING_PROCESS_"));
    assert!(INHERITED_LISTENER_FD_ENV.starts_with("RUNNING_PROCESS_"));
}

#[test]
fn no_environment_value_means_no_inherited_listener() {
    if std::env::var_os(INHERITED_LISTENER_FD_ENV).is_some() {
        eprintln!("skipping: inherited-listener environment is set");
        return;
    }
    assert!(recover_from_env()
        .expect("absence is not an error")
        .is_none());
}

#[test]
fn support_query_matches_the_platform_capability() {
    assert_eq!(support().is_supported(), InheritableListener::supported());
}

/// Child half of `prepared_listener_survives_sanitized_daemon_exec`.
///
/// A command-local marker keeps ordinary unit-test execution inert. The
/// parent spawns this exact test in a fresh process so recovery happens only
/// after a real fork+exec boundary.
#[test]
fn inherited_listener_daemon_child() {
    let Some(expectation) = std::env::var_os(INHERITED_LISTENER_CHILD_ENV) else {
        return;
    };

    if expectation == CHILD_EXPECTS_CLOSED {
        assert!(
            recover_from_env().is_err(),
            "ordinary exec must not recover the CLOEXEC listener"
        );
        return;
    }
    assert_eq!(expectation, CHILD_EXPECTS_INHERITED);

    use std::io::{Read as _, Write as _};

    let listener = recover_from_env()
        .expect("recover prepared listener after daemon exec")
        .expect("child receives prepared listener contract");
    let mut stream = listener.accept().expect("accept inherited listener");
    let mut request = [0_u8; 4];
    stream
        .read_exact(&mut request)
        .expect("read parent request");
    assert_eq!(&request, b"ping");
    stream.write_all(b"pong").expect("write child response");
    stream.flush().expect("flush child response");
}

#[test]
fn prepared_listener_survives_sanitized_daemon_exec() {
    if !InheritableListener::supported() {
        return;
    }

    use std::io::{Read as _, Write as _};
    use std::process::Command;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use crate::client::{IpcEndpoint, IpcStream};

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let endpoint = format!("/tmp/rp-inherit-{}-{nonce:x}.sock", std::process::id());
    let listener = InheritableListener::bind(&endpoint).expect("bind broker-owned listener");

    let child_command = |expectation: &str| {
        let mut command = Command::new(std::env::current_exe().expect("current unit-test binary"));
        command
            .arg("--exact")
            .arg(CHILD_TEST_NAME)
            .arg("--nocapture")
            .env(INHERITED_LISTENER_CHILD_ENV, expectation);
        command
    };

    // Preparation must never make the listener inheritable by an unrelated
    // exec. Prove that behavior at the process boundary rather than inspecting
    // the native descriptor from broker code.
    let mut ordinary_command = child_command(CHILD_EXPECTS_CLOSED);
    let _ordinary_inheritance = listener
        .prepare_for_daemon(&mut ordinary_command)
        .expect("prepare listener while retaining CLOEXEC in the parent");
    let ordinary_status = ordinary_command
        .status()
        .expect("spawn ordinary child without the daemon inheritance hook");
    assert!(
        ordinary_status.success(),
        "ordinary child must confirm that the listener stayed closed"
    );

    let mut command = child_command(CHILD_EXPECTS_INHERITED);
    let inheritance = listener
        .prepare_for_daemon(&mut command)
        .expect("prepare listener inheritance");
    let mut child = crate::spawn::spawn_daemon_with_inheritance(&mut command, inheritance)
        .expect("spawn sanitized daemon with prepared listener");

    let exchange = (|| -> std::io::Result<()> {
        let ipc_endpoint = IpcEndpoint::new(endpoint)?;
        let mut stream = IpcStream::connect(&ipc_endpoint)?;
        stream.set_recv_timeout(Some(Duration::from_secs(5)))?;
        stream.write_all(b"ping")?;
        stream.flush()?;
        let mut response = [0_u8; 4];
        stream.read_exact(&mut response)?;
        if &response != b"pong" {
            return Err(std::io::Error::other(
                "unexpected inherited-listener response",
            ));
        }
        Ok(())
    })();

    if exchange.is_err() {
        let _ = child.kill();
    }
    let exit_code = child.wait().expect("wait for inherited-listener child");
    exchange.expect("real child recovered and accepted inherited listener");
    assert_eq!(exit_code, 0, "inherited-listener child must exit cleanly");
}
