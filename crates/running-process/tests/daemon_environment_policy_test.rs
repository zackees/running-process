#![cfg(feature = "daemon")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use running_process::daemon::client::{DaemonClient, SpawnCommandRequest};
use running_process::daemon::paths;
use running_process::daemon::pipe_session::{PipeSpawnRequest, PipeStreamAttachment};
use running_process::daemon::pty_session::{PtyAttachment, PtySpawnRequest};
use running_process::daemon::server::DaemonServer;
use running_process::proto::daemon::{pty_stream_frame::Frame as PtyStreamOneof, PipeStreamKind};
use running_process::{EnvironmentPolicy, DAEMON_MARKER_ENV_VAR, ORIGINATOR_ENV_VAR};

const CLIENT_KEY: &str = "RP_POLICY_CLIENT_ONLY";
const CLIENT_VALUE: &str = "forwarded";

struct EnvVarGuard {
    key: String,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &str, value: &str) -> Self {
        let guard = Self {
            key: key.to_owned(),
            previous: std::env::var_os(key),
        };
        std::env::set_var(key, value);
        guard
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(&self.key, value),
            None => std::env::remove_var(&self.key),
        }
    }
}

#[cfg(windows)]
const BASELINE_KEY: &str = "USERNAME";
#[cfg(unix)]
const BASELINE_KEY: &str = "HOME";

fn testbin_path(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("test binary should live in <profile>/deps");
    let path = profile_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(path.is_file(), "missing test fixture: {}", path.display());
    path
}

fn start_server(scope: &str) -> (tokio::task::JoinHandle<()>, String, tempfile::TempDir) {
    let socket = paths::socket_path(Some(scope));
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp
        .path()
        .join("registry.db")
        .to_string_lossy()
        .into_owned();
    let server = DaemonServer::new(
        socket.clone(),
        db,
        "environment-policy-test".into(),
        scope.into(),
        std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    )
    .expect("DaemonServer::new");
    let handle = tokio::spawn(async move { server.run().await.expect("server.run") });
    (handle, socket, temp)
}

fn with_ambient_marker<T>(key: &str, operation: impl FnOnce() -> T) -> T {
    let _guard = EnvVarGuard::set(key, "daemon-only");
    operation()
}

fn assert_common_probe(text: &str, originator: &str) {
    assert!(
        text.contains(&format!("ENV:{CLIENT_KEY}={CLIENT_VALUE}")),
        "client value missing from probe"
    );
    assert!(
        text.contains(&format!("ORIGINATOR={originator}")),
        "allowlisted originator metadata missing from probe"
    );
}

fn assert_policy_probe(text: &str, daemon_key: &str, inherited: bool) {
    let expected = if inherited { "daemon-only" } else { "<unset>" };
    assert!(
        text.contains(&format!("ENV:{daemon_key}={expected}")),
        "unexpected daemon ambient value visibility"
    );
}

fn reporter_argv(reporter: &Path, daemon_key: &str) -> Vec<String> {
    vec![
        reporter.to_string_lossy().into_owned(),
        CLIENT_KEY.into(),
        daemon_key.into(),
        BASELINE_KEY.into(),
    ]
}

#[cfg(windows)]
fn answer_new_cursor_queries(
    attachment: &mut PtyAttachment,
    output: &[u8],
    answered_queries: &mut usize,
) {
    let query_count = output
        .windows(b"\x1b[6n".len())
        .filter(|window| *window == b"\x1b[6n")
        .count();
    while *answered_queries < query_count {
        attachment
            .send_input(b"\x1b[1;1R")
            .expect("answer ConPTY cursor query");
        *answered_queries += 1;
    }
}

fn collect_pty_probe(mut attachment: PtyAttachment) -> String {
    let mut output = attachment.initial_backlog.clone();
    #[cfg(windows)]
    let mut answered_queries = 0;
    #[cfg(windows)]
    answer_new_cursor_queries(&mut attachment, &output, &mut answered_queries);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !output
        .windows(b"READY".len())
        .any(|window| window == b"READY")
    {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for PTY probe; captured {}",
            String::from_utf8_lossy(&output)
        );
        if let Some(frame) = attachment
            .recv_frame_with_timeout(Duration::from_millis(250))
            .expect("receive PTY probe")
        {
            match frame.frame {
                Some(PtyStreamOneof::Output(bytes)) => {
                    output.extend_from_slice(&bytes);
                    #[cfg(windows)]
                    answer_new_cursor_queries(&mut attachment, &output, &mut answered_queries);
                }
                Some(PtyStreamOneof::Error(message)) => panic!("PTY probe error: {message}"),
                Some(PtyStreamOneof::ExitCode(code)) => {
                    panic!("PTY probe exited before READY: {code}")
                }
                _ => {}
            }
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn parse_env_dump(path: &Path) -> HashMap<String, String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let contents = std::fs::read_to_string(path).expect("read environment dump");
    contents
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn shell_quote(path: &Path) -> String {
    format!("\"{}\"", path.display())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detached_child_enforces_all_environment_policies_and_metadata() {
    const DAEMON_KEY: &str = "RP_POLICY_DETACHED_DAEMON_ONLY";
    let scope = format!("env-policy-detached-{}", line!());
    let (server, socket, _temp) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let dump = testbin_path("testbin-env-dump");
    let output_dir = tempfile::tempdir().expect("output tempdir");

    let socket_for_test = socket.clone();
    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");
        for (name, policy, inherits) in [
            ("default", None, false),
            ("inherit", Some(EnvironmentPolicy::Inherit), true),
            ("baseline", Some(EnvironmentPolicy::UserBaseline), false),
        ] {
            let output = output_dir.path().join(format!("{name}.env"));
            let command = format!("{} {}", shell_quote(&dump), shell_quote(&output));
            let request = SpawnCommandRequest::shell(command)
                .with_env(CLIENT_KEY, CLIENT_VALUE)
                .with_originator("detached-policy");
            let request = policy
                .map(|value| request.clone().with_environment_policy(value))
                .unwrap_or(request);
            with_ambient_marker(DAEMON_KEY, || {
                client.spawn_command(&request).expect("spawn detached")
            });
            let env = parse_env_dump(&output);
            assert_eq!(env.get(CLIENT_KEY).map(String::as_str), Some(CLIENT_VALUE));
            assert_eq!(env.contains_key(DAEMON_KEY), inherits);
            assert_eq!(
                env.get(ORIGINATOR_ENV_VAR).map(String::as_str),
                Some("detached-policy")
            );
            assert_eq!(
                env.get(DAEMON_MARKER_ENV_VAR).map(String::as_str),
                Some("1")
            );
            if policy == Some(EnvironmentPolicy::UserBaseline) {
                assert!(
                    env.contains_key(BASELINE_KEY),
                    "baseline identity key missing"
                );
            }
        }
        client.shutdown(true, 5.0).expect("shutdown");
    })
    .await
    .expect("blocking task");
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("server shutdown timeout")
        .expect("server task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipe_child_enforces_all_environment_policies_and_metadata() {
    const DAEMON_KEY: &str = "RP_POLICY_PIPE_DAEMON_ONLY";
    let scope = format!("env-policy-pipe-{}", line!());
    let (server, socket, _temp) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let reporter = testbin_path("testbin-env-reporter");

    let socket_for_test = socket.clone();
    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");
        for (policy, inherits) in [
            (None, false),
            (Some(EnvironmentPolicy::Inherit), true),
            (Some(EnvironmentPolicy::UserBaseline), false),
        ] {
            let request = PipeSpawnRequest::new(reporter_argv(&reporter, DAEMON_KEY))
                .with_envs([(CLIENT_KEY, CLIENT_VALUE)])
                .with_originator("pipe-policy");
            let request = policy
                .map(|value| request.clone().with_environment_policy(value))
                .unwrap_or(request);
            let spawned = with_ambient_marker(DAEMON_KEY, || {
                client.spawn_pipe_session(&request).expect("spawn pipe")
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let snapshot = client
                    .get_session_backlog(&spawned.session_id, PipeStreamKind::Stdout)
                    .expect("snapshot pipe probe")
                    .expect("pipe session present");
                if snapshot
                    .backlog
                    .windows(b"READY".len())
                    .any(|window| window == b"READY")
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for pipe probe; captured {}",
                    String::from_utf8_lossy(&snapshot.backlog)
                );
                std::thread::sleep(Duration::from_millis(25));
            }
            let attachment = PipeStreamAttachment::attach_to(
                &socket_for_test,
                &spawned.session_id,
                PipeStreamKind::Stdout,
                false,
            )
            .expect("attach pipe");
            let text = String::from_utf8_lossy(&attachment.initial_backlog);
            assert_common_probe(&text, "pipe-policy");
            assert_policy_probe(&text, DAEMON_KEY, inherits);
            if policy == Some(EnvironmentPolicy::UserBaseline) {
                assert!(!text.contains(&format!("ENV:{BASELINE_KEY}=<unset>")));
            }
            drop(attachment);
            client
                .terminate_pipe_session(&spawned.session_id, 1000)
                .expect("terminate pipe");
        }
        client.shutdown(true, 5.0).expect("shutdown");
    })
    .await
    .expect("blocking task");
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("server shutdown timeout")
        .expect("server task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pty_child_enforces_all_environment_policies_and_metadata() {
    const DAEMON_KEY: &str = "RP_POLICY_PTY_DAEMON_ONLY";
    let scope = format!("env-policy-pty-{}", line!());
    let (server, socket, _temp) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let reporter = testbin_path("testbin-env-reporter");

    let socket_for_test = socket.clone();
    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");
        for (policy, inherits) in [
            (None, false),
            (Some(EnvironmentPolicy::Inherit), true),
            (Some(EnvironmentPolicy::UserBaseline), false),
        ] {
            let request = PtySpawnRequest::new(reporter_argv(&reporter, DAEMON_KEY))
                .with_envs([(CLIENT_KEY, CLIENT_VALUE)])
                .with_originator("pty-policy");
            let request = policy
                .map(|value| request.clone().with_environment_policy(value))
                .unwrap_or(request);
            let spawned = with_ambient_marker(DAEMON_KEY, || {
                client.spawn_pty_session(&request).expect("spawn pty")
            });
            std::thread::sleep(Duration::from_millis(300));
            let attachment =
                PtyAttachment::attach_to(&socket_for_test, &spawned.session_id, 24, 80, false)
                    .expect("attach pty");
            let text = collect_pty_probe(attachment);
            assert_common_probe(&text, "pty-policy");
            assert_policy_probe(&text, DAEMON_KEY, inherits);
            if policy == Some(EnvironmentPolicy::UserBaseline) {
                assert!(!text.contains(&format!("ENV:{BASELINE_KEY}=<unset>")));
            }
            client
                .terminate_pty_session(&spawned.session_id, 1000)
                .expect("terminate pty");
        }
        client.shutdown(true, 5.0).expect("shutdown");
    })
    .await
    .expect("blocking task");
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("server shutdown timeout")
        .expect("server task");
}
