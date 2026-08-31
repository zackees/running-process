//! Real-process conformance for broker and backend replacement.
//!
//! The fixture backend is a separate OS process speaking the public backend
//! probe and frame contracts. These tests deliberately stay generic: service
//! names, routes, and replacement policy come entirely from running-process.

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use running_process::broker::backend_lifecycle::verify_pid;
use running_process::broker::backend_sdk::{
    read_daemon_identity_file, remove_daemon_identity_file, FrameClient,
};
use running_process::broker::client::{
    connect_to_backend, BrokerClientError, ConnectBackendRequest,
};
use running_process::broker::client_v2;
use running_process::broker::lifecycle::names_v2::daemon_identity_path;
use running_process::broker::protocol::{Endpoint, ErrorCode};
use running_process::broker::protocol_v2::ServiceDefinitionBuilder;
use running_process::broker::server::{
    ensure_service_definition_dir, serve_launching_backends, BrokerLaunchServeConfig,
    BACKEND_ENV_ENDPOINT_NAMESPACE, BACKEND_ENV_ENDPOINT_PATH, BACKEND_ENV_INSTANCE,
    BACKEND_ENV_SERVICE_NAME, BACKEND_ENV_SERVICE_VERSION,
};
use running_process::client::IpcEndpoint;

const VERSION: &str = "1.0.0";
const LIFECYCLE_TEST_PAYLOAD_PROTOCOL: u32 = 0xF824;
const DEADLINE: Duration = Duration::from_secs(20);
const RACERS: usize = 8;
const CONNECTION_BUDGET: usize = 128;

#[test]
fn broker_restart_re_adopts_live_backend_and_serves_next_client() {
    let service = unique_name("broker-restart");
    let endpoint = crate::socket_common::unique_socket_name("broker-restart-backend");
    let identity_path = daemon_identity_path(&service);
    remove_daemon_identity_file(&identity_path);
    let _identity_cleanup = IdentityFileCleanup(identity_path);

    let mut backend = ChildGuard::spawn_backend(&service, &endpoint);
    let backend_pid = await_published_backend(&service, backend.id());

    let first_broker = BrokerV2Guard::start(&service);
    assert_eq!(request_via_v2(&service, &endpoint), backend_pid);
    first_broker.crash_leaving_stale_endpoint();

    let replacement_broker = BrokerV2Guard::start(&service);
    assert_eq!(
        request_via_v2(&service, &endpoint),
        backend_pid,
        "a replacement broker must rediscover the already-live backend"
    );
    assert_eq!(
        await_published_backend(&service, backend.id()),
        backend_pid,
        "broker replacement must not replace the backend process"
    );

    drop(replacement_broker);
    let _ = backend.kill();
    let _ = backend.wait();
}

#[test]
fn backend_crash_concurrent_reconnects_launch_one_replacement_without_disturbing_other_instance() {
    let service_a = unique_name("backend-crash-a");
    let service_b = unique_name("backend-crash-b");
    let identity_a = daemon_identity_path(&service_a);
    let identity_b = daemon_identity_path(&service_b);
    remove_daemon_identity_file(&identity_a);
    remove_daemon_identity_file(&identity_b);

    let cleanup = ProcessCleanup::new([service_a.clone(), service_b.clone()]);
    let definitions = install_service_definitions([service_a.as_str(), service_b.as_str()]);
    let broker_endpoint = crate::socket_common::unique_socket_name("lifecycle-launch-broker");
    let config = BrokerLaunchServeConfig::new(&broker_endpoint, CONNECTION_BUDGET)
        .expect("bounded lifecycle broker config")
        .with_service_definition_dir(definitions.path().join("services"));
    let server = thread::spawn(move || serve_launching_backends(config));
    let accepted_negotiations = Arc::new(AtomicUsize::new(0));

    let old_a = round_trip_until_served(
        &broker_endpoint,
        &service_a,
        &accepted_negotiations,
        "initial route for service under recovery",
    );
    let stable_b = round_trip_until_served(
        &broker_endpoint,
        &service_b,
        &accepted_negotiations,
        "initial route for independent service",
    );
    cleanup.track(old_a);
    cleanup.track(stable_b);
    cleanup.track_published(&service_a);
    cleanup.track_published(&service_b);

    running_process::process_tree::kill_tree(old_a, Duration::from_millis(100))
        .expect("kill crashed backend fixture");
    await_process_exit(old_a);

    let barrier = Arc::new(Barrier::new(RACERS));
    let mut first_wave = Vec::with_capacity(RACERS);
    for _ in 0..RACERS {
        let endpoint = broker_endpoint.clone();
        let service = service_a.clone();
        let barrier = Arc::clone(&barrier);
        let accepted_negotiations = Arc::clone(&accepted_negotiations);
        first_wave.push(thread::spawn(move || {
            barrier.wait();
            round_trip(&endpoint, &service, &accepted_negotiations)
        }));
    }
    let first_wave: Vec<_> = first_wave
        .into_iter()
        .map(|worker| worker.join().expect("reconnect worker"))
        .collect();
    assert!(
        first_wave.iter().any(|outcome| matches!(
            outcome,
            ReconnectOutcome::Served(_) | ReconnectOutcome::BackendTransition(_)
        )),
        "one reconnect must own the replacement attempt"
    );
    assert!(
        first_wave.iter().all(|outcome| matches!(
            outcome,
            ReconnectOutcome::Served(_)
                | ReconnectOutcome::BackendTransition(_)
                | ReconnectOutcome::RateLimited(_)
        )),
        "same-key contenders may only serve, observe the stale-route transition, or observe \
         single-flight backpressure"
    );

    let second_wave: Vec<u32> = (0..RACERS)
        .map(|_| {
            round_trip_until_served(
                &broker_endpoint,
                &service_a,
                &accepted_negotiations,
                "settled replacement route",
            )
        })
        .collect();
    let replacement = second_wave[0];
    assert_ne!(replacement, old_a, "the dead process cannot be reused");
    assert!(
        second_wave.iter().all(|pid| *pid == replacement),
        "all reconnects must converge on one replacement process: {second_wave:?}"
    );
    assert!(first_wave.iter().all(|outcome| match outcome {
        ReconnectOutcome::Served(pid) => *pid == replacement,
        ReconnectOutcome::BackendTransition(_) | ReconnectOutcome::RateLimited(_) => true,
        ReconnectOutcome::BrokerStarting => false,
    }));
    cleanup.track(replacement);
    cleanup.track_published(&service_a);

    assert_eq!(
        round_trip_until_served(
            &broker_endpoint,
            &service_b,
            &accepted_negotiations,
            "independent route after replacement",
        ),
        stable_b,
        "replacing one backend must not displace another service"
    );
    drain_connection_budget(
        &broker_endpoint,
        &service_b,
        stable_b,
        &accepted_negotiations,
    );

    server
        .join()
        .expect("lifecycle broker thread")
        .expect("lifecycle broker serve result");
    drop(cleanup);
}

fn request_via_v2(service: &str, expected_endpoint: &str) -> u32 {
    let session = client_v2::connect_with_deadline(service, VERSION, DEADLINE)
        .expect("v2 broker Hello after startup");
    assert_eq!(session.negotiated().backend_pipe, expected_endpoint);
    let endpoint = Endpoint {
        namespace_id: String::new(),
        path: session.negotiated().backend_pipe.clone(),
    };
    let mut client = FrameClient::connect(&endpoint).expect("dial adopted backend route");
    response_pid(
        client
            .request(LIFECYCLE_TEST_PAYLOAD_PROTOCOL, b"ping".to_vec())
            .expect("request through adopted backend route")
            .payload,
    )
}

fn round_trip_until_served(
    broker_endpoint: &str,
    service: &str,
    accepted_negotiations: &AtomicUsize,
    context: &str,
) -> u32 {
    let deadline = Instant::now() + DEADLINE;
    loop {
        match round_trip(broker_endpoint, service, accepted_negotiations) {
            ReconnectOutcome::Served(pid) => return pid,
            ReconnectOutcome::BackendTransition(retry_after)
            | ReconnectOutcome::RateLimited(retry_after)
                if Instant::now() < deadline =>
            {
                sleep_for_retry(retry_after, deadline);
            }
            ReconnectOutcome::BackendTransition(_) | ReconnectOutcome::RateLimited(_) => {
                panic!("{context}: retryable lifecycle transition did not settle in time")
            }
            ReconnectOutcome::BrokerStarting if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            ReconnectOutcome::BrokerStarting => {
                panic!("{context}: lifecycle broker did not bind in time")
            }
        }
    }
}

fn round_trip(
    broker_endpoint: &str,
    service: &str,
    accepted_negotiations: &AtomicUsize,
) -> ReconnectOutcome {
    let request = ConnectBackendRequest::new(broker_endpoint, service, VERSION, VERSION);
    let result = connect_to_backend(request);
    if !matches!(&result, Err(BrokerClientError::BrokerConnect(_))) {
        let accepted = accepted_negotiations.fetch_add(1, Ordering::Relaxed) + 1;
        assert!(
            accepted <= CONNECTION_BUDGET,
            "lifecycle test exceeded its bounded broker connection budget"
        );
    }
    let connection = match result {
        Ok(connection) => connection,
        Err(BrokerClientError::BrokerConnect(_)) => return ReconnectOutcome::BrokerStarting,
        Err(BrokerClientError::Refused {
            code: ErrorCode::ErrorBackendSpawnFailed,
            retry_after_ms,
            ..
        }) if retry_after_ms > 0 => {
            return ReconnectOutcome::BackendTransition(Duration::from_millis(retry_after_ms));
        }
        Err(BrokerClientError::Refused {
            code: ErrorCode::ErrorRateLimited,
            retry_after_ms,
            ..
        }) => {
            return ReconnectOutcome::RateLimited(Duration::from_millis(retry_after_ms));
        }
        Err(error) => panic!("unexpected lifecycle negotiation failure: {error}"),
    };
    let mut client = FrameClient::from_stream(connection.stream);
    let response = client
        .request(LIFECYCLE_TEST_PAYLOAD_PROTOCOL, b"ping".to_vec())
        .expect("real backend request after negotiation");
    ReconnectOutcome::Served(response_pid(response.payload))
}

fn sleep_for_retry(retry_after: Duration, deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return;
    }
    thread::sleep(retry_after.max(Duration::from_millis(10)).min(remaining));
}

fn drain_connection_budget(
    broker_endpoint: &str,
    stable_service: &str,
    stable_pid: u32,
    accepted_negotiations: &AtomicUsize,
) {
    while accepted_negotiations.load(Ordering::Relaxed) < CONNECTION_BUDGET {
        assert_eq!(
            round_trip(broker_endpoint, stable_service, accepted_negotiations)
                .expect_served("drain bounded broker through stable route"),
            stable_pid,
            "connection-budget drain must not replace the independent backend"
        );
    }
    assert_eq!(
        accepted_negotiations.load(Ordering::Relaxed),
        CONNECTION_BUDGET
    );
}

fn response_pid(payload: Vec<u8>) -> u32 {
    let bytes: [u8; 4] = payload
        .try_into()
        .expect("lifecycle fixture response is a big-endian u32 PID");
    u32::from_be_bytes(bytes)
}

#[derive(Clone, Copy, Debug)]
enum ReconnectOutcome {
    Served(u32),
    BackendTransition(Duration),
    RateLimited(Duration),
    BrokerStarting,
}

impl ReconnectOutcome {
    fn expect_served(self, context: &str) -> u32 {
        match self {
            Self::Served(pid) => pid,
            other => panic!("{context}: expected served route, got {other:?}"),
        }
    }
}

fn install_service_definitions<'a>(
    services: impl IntoIterator<Item = &'a str>,
) -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("lifecycle service definitions");
    let root = temp.path().join("services");
    ensure_service_definition_dir(&root).expect("private service definition directory");
    let backend =
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_running-process-lifecycle-test-backend"));
    let binary_root = backend.parent().expect("backend binary directory");
    for (index, service) in services.into_iter().enumerate() {
        ServiceDefinitionBuilder::explicit_instance(
            service,
            backend.display().to_string(),
            format!("lifecycle-{index}"),
        )
        .per_version_binary_dir(binary_root.display().to_string())
        .min_version(VERSION)
        .version_allow_list([VERSION])
        .install_in(&root)
        .expect("install lifecycle service definition");
    }
    temp
}

fn await_published_backend(service: &str, expected_pid: u32) -> u32 {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(identity) = read_daemon_identity_file(&daemon_identity_path(service)) {
            if identity.pid == expected_pid && verify_pid::process_is_alive(identity.pid) {
                return identity.pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "backend {expected_pid} did not publish a live identity for {service}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn await_process_exit(pid: u32) {
    let deadline = Instant::now() + DEADLINE;
    while verify_pid::process_is_alive(pid) {
        assert!(Instant::now() < deadline, "process {pid} did not exit");
        thread::sleep(Duration::from_millis(10));
    }
}

fn unique_name(prefix: &str) -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{:010x}", nonce & 0xFF_FFFF_FFFF)
}

struct BrokerV2Guard {
    child: Child,
    endpoint: IpcEndpoint,
    retire_on_drop: bool,
    _service_definitions: tempfile::TempDir,
}

impl BrokerV2Guard {
    fn start(service: &str) -> Self {
        let definitions = install_service_definitions([service]);
        let mut child = Command::new(env!("CARGO_BIN_EXE_running-process-broker-v2"))
            .arg("--program")
            .arg(service)
            .env("RUNNING_PROCESS_BROKER_ALLOW_PRIVILEGED", "1")
            .env(
                "RUNNING_PROCESS_SERVICE_DEF_DIR",
                definitions.path().join("services"),
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn v2 lifecycle broker");
        let stdout = child.stdout.take().expect("broker stdout");
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut sent = false;
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if !sent {
                    if let Some(rest) = line.strip_prefix("running-process-broker-v2 bound at ") {
                        let rest = rest.trim_end();
                        let endpoint = rest
                            .rsplit_once(" (")
                            .map(|(path, _)| path)
                            .unwrap_or(rest)
                            .to_string();
                        let _ = ready_tx.send(endpoint);
                        sent = true;
                    }
                }
            }
        });
        let endpoint = ready_rx
            .recv_timeout(DEADLINE)
            .expect("v2 lifecycle broker bound in time");
        Self {
            child,
            endpoint: IpcEndpoint::new(endpoint).expect("v2 broker endpoint"),
            retire_on_drop: true,
            _service_definitions: definitions,
        }
    }

    fn crash_leaving_stale_endpoint(mut self) {
        self.retire_on_drop = false;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for BrokerV2Guard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if self.retire_on_drop {
            let _ = self.endpoint.retire();
        }
    }
}

struct ChildGuard {
    child: Child,
    endpoint: IpcEndpoint,
}

impl ChildGuard {
    fn spawn_backend(service: &str, endpoint: &str) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_running-process-lifecycle-test-backend"))
            .env(BACKEND_ENV_SERVICE_NAME, service)
            .env(BACKEND_ENV_SERVICE_VERSION, VERSION)
            .env(BACKEND_ENV_ENDPOINT_PATH, endpoint)
            .env(BACKEND_ENV_ENDPOINT_NAMESPACE, "shared")
            .env(BACKEND_ENV_INSTANCE, "shared")
            .env_remove(broker_owned_bind_env())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn lifecycle backend");
        Self {
            child,
            endpoint: IpcEndpoint::new(endpoint.to_string()).expect("backend endpoint"),
        }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill()
    }

    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = self.endpoint.retire();
    }
}

fn broker_owned_bind_env() -> &'static str {
    running_process::broker::broker_owned_bind::INHERITED_LISTENER_FD_ENV
}

struct ProcessCleanup {
    pids: Mutex<HashSet<u32>>,
    endpoints: Mutex<HashSet<String>>,
    services: Vec<String>,
}

impl ProcessCleanup {
    fn new(services: impl IntoIterator<Item = String>) -> Self {
        Self {
            pids: Mutex::new(HashSet::new()),
            endpoints: Mutex::new(HashSet::new()),
            services: services.into_iter().collect(),
        }
    }

    fn track(&self, pid: u32) {
        self.pids.lock().expect("cleanup PID set").insert(pid);
    }

    fn track_published(&self, service: &str) {
        if let Some(identity) = read_daemon_identity_file(&daemon_identity_path(service)) {
            self.track(identity.pid);
            self.endpoints
                .lock()
                .expect("cleanup endpoint set")
                .insert(identity.ipc_endpoint.path);
        }
    }
}

impl Drop for ProcessCleanup {
    fn drop(&mut self) {
        let mut pids = self
            .pids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for service in &self.services {
            let path = daemon_identity_path(service);
            if let Some(identity) = read_daemon_identity_file(&path) {
                pids.insert(identity.pid);
                self.endpoints
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(identity.ipc_endpoint.path);
            }
            remove_daemon_identity_file(&path);
        }
        for pid in pids.drain() {
            let _ = running_process::process_tree::kill_tree(pid, Duration::from_millis(100));
        }
        for endpoint in self
            .endpoints
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain()
        {
            if let Ok(endpoint) = IpcEndpoint::new(endpoint) {
                let _ = endpoint.retire();
            }
        }
    }
}

struct IdentityFileCleanup(std::path::PathBuf);

impl Drop for IdentityFileCleanup {
    fn drop(&mut self) {
        remove_daemon_identity_file(&self.0);
    }
}
