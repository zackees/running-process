//! Real pre-existing broker integration. Docker coverage is a separate fixture.
#![cfg(all(target_os = "linux", feature = "independent-spawn"))]
use running_process::{
    independent_spawn::{spawn, IndependentChild, LaunchSpec, Readiness},
    spawn_with_options, IndependentBackend, SpawnMode, SpawnOptions,
};
use std::{
    fs,
    path::Path,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

struct Cleanup(IndependentChild);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.0.stop(Duration::from_secs(5));
    }
}

#[test]
fn absent_or_in_worker_broker_fails_before_payload_transfer() {
    use std::io::Read;
    let directory = tempfile::tempdir().unwrap();
    let endpoint = directory.path().join("broker.sock");
    let spec = LaunchSpec {
        program: "/bin/sh".into(),
        args: vec!["secret must not reach an in-worker broker".into()],
        cwd: "/".into(),
        environment: vec![],
        stdout: None,
        stderr: None,
        readiness: Readiness::ProcessStarted,
    };
    let options = SpawnOptions {
        mode: SpawnMode::Independent,
        backend: Some(IndependentBackend::ExternalBroker {
            endpoint: endpoint.to_str().unwrap().to_owned(),
        }),
        timeout: Duration::from_secs(2),
        ..SpawnOptions::default()
    };
    assert_eq!(
        spawn_with_options(&spec, &options, &AtomicBool::new(false))
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    assert!(!endpoint.exists());
    let listener = std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
    let peer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream.read(&mut [0_u8; 1]).unwrap()
    });
    assert_eq!(
        spawn_with_options(&spec, &options, &AtomicBool::new(false))
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    assert_eq!(
        peer.join().unwrap(),
        0,
        "placement must be checked before sending any payload byte"
    );
}

#[test]
#[ignore = "requires an accessible systemd user manager to pre-provision the outside broker"]
fn existing_broker_preserves_payload_and_places_target_outside_worker() {
    let directory = tempfile::tempdir().unwrap();
    let endpoint = directory.path().join("broker").join("s");
    let helper = Path::new(env!("CARGO_BIN_EXE_running-process-launcher"));
    let cancelled = AtomicBool::new(false);
    let broker_spec = LaunchSpec {
        program: helper.as_os_str().to_owned(),
        args: vec!["--broker".into(), endpoint.as_os_str().to_owned()],
        cwd: directory.path().as_os_str().to_owned(),
        environment: vec![],
        stdout: None,
        stderr: None,
        readiness: Readiness::ProcessStarted,
    };
    let mut broker =
        Cleanup(spawn(&broker_spec, helper, Duration::from_secs(10), &cancelled).unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !endpoint.exists() {
        assert!(broker.0.is_alive(), "provisioned broker exited");
        assert!(
            Instant::now() < deadline,
            "broker did not bind its endpoint"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let argument = "argument with spaces ' and Ω";
    let token = "selected value with spaces and $symbols";
    let ready = directory.path().join("ready");
    let stdout = directory.path().join("stdout");
    let stderr = directory.path().join("stderr");
    let spec = LaunchSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "printf '%s\\n' \"$1\" \"$TOKEN\" \"$PWD\"; printf diagnostic >&2; printf ready > \"$READY\"; exec sleep 30".into(), "broker-target".into(), argument.into()],
        cwd: directory.path().as_os_str().to_owned(),
        environment: vec![("TOKEN".into(), token.into()), ("READY".into(), ready.as_os_str().to_owned()), ("PATH".into(), std::env::var_os("PATH").unwrap())],
        stdout: Some(stdout.as_os_str().to_owned()), stderr: Some(stderr.as_os_str().to_owned()),
        readiness: Readiness::File { path: ready.into_os_string(), value: b"ready".to_vec() },
    };
    let options = SpawnOptions {
        mode: SpawnMode::Independent,
        backend: Some(IndependentBackend::ExternalBroker {
            endpoint: endpoint.to_str().unwrap().to_owned(),
        }),
        timeout: Duration::from_secs(5),
        ..SpawnOptions::default()
    };
    let mut child = spawn_with_options(&spec, &options, &cancelled).unwrap();
    assert_eq!(child.actual_mode(), SpawnMode::Independent);
    assert_eq!(
        fs::read_to_string(stdout).unwrap(),
        format!("{argument}\n{token}\n{}\n", directory.path().display())
    );
    assert_eq!(fs::read(stderr).unwrap(), b"diagnostic");
    let worker = fs::read_to_string("/proc/self/cgroup").unwrap();
    let target = fs::read_to_string(format!("/proc/{}/cgroup", child.id())).unwrap();
    assert_ne!(target, worker);
    assert!(!target.trim().starts_with(&format!("{}/", worker.trim())));
    child.stop(Duration::from_secs(5)).unwrap();
    assert!(!child.is_alive().unwrap());
    broker.0.stop(Duration::from_secs(5)).unwrap();
}
