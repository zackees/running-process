//! Opt-in runtime fixture for a private, delegated Docker cgroup namespace.
#![cfg(all(target_os = "linux", feature = "independent-spawn"))]
use running_process::{
    independent_spawn::{LaunchSpec, Readiness},
    spawn_with_options, IndependentBackend, SpawnHandle, SpawnLifetime, SpawnMode, SpawnOptions,
};
use std::{
    fs,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

const CG: &str = "/sys/fs/cgroup";
const ALLOCATION: usize = 24 * 1024 * 1024;

fn wait_for(phase: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "Docker fixture deadline expired: {phase}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn group(pid: u32) -> String {
    fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .unwrap()
        .trim()
        .strip_prefix("0::")
        .unwrap()
        .to_owned()
}
fn ticks(pid: u32) -> u64 {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    stat.rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap()
}
fn memory(group: &str) -> u64 {
    fs::read_to_string(format!("{CG}/{group}/memory.current"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}
fn event(file: &str, key: &str) -> u64 {
    fs::read_to_string(format!("{CG}/{file}"))
        .unwrap()
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(' ')?;
            (name == key).then(|| value.parse().unwrap())
        })
        .unwrap()
}
fn fixture(name: &str, directory: &Path, marker: Option<&Path>) -> LaunchSpec {
    LaunchSpec {
        program: std::env::current_exe().unwrap().into_os_string(),
        args: vec![
            "--exact".into(),
            name.into(),
            "--ignored".into(),
            "--nocapture".into(),
        ],
        cwd: directory.as_os_str().to_owned(),
        environment: vec![(
            "RP_DOCKER_DIRECTORY".into(),
            directory.as_os_str().to_owned(),
        )],
        stdout: None,
        stderr: None,
        readiness: marker.map_or(Readiness::ProcessStarted, |path| Readiness::File {
            path: path.as_os_str().to_owned(),
            value: b"ready".to_vec(),
        }),
    }
}
fn start(spec: &LaunchSpec, options: &SpawnOptions) -> SpawnHandle {
    spawn_with_options(spec, options, &AtomicBool::new(false)).unwrap()
}

struct Identity(OwnedFd);
impl Identity {
    fn pin(pid: u32, expected_ticks: u64) -> Self {
        // SAFETY: pidfd_open returns a uniquely owned fd on success.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        assert!(fd >= 0, "pidfd_open failed");
        // SAFETY: fd is the newly returned pidfd and is owned only here.
        let identity = Self(unsafe { OwnedFd::from_raw_fd(fd as i32) });
        assert_eq!(ticks(pid), expected_ticks, "reported PID was reused");
        assert!(identity.alive());
        identity
    }
    fn alive(&self) -> bool {
        let mut event = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: event is one initialized pollfd referring to an owned fd.
        let result = unsafe { libc::poll(&mut event, 1, 0) };
        assert!(result >= 0);
        result == 0
    }
}
impl Drop for Identity {
    fn drop(&mut self) {
        // SAFETY: the pinned fd cannot redirect termination to a reused PID.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
    }
}

#[test]
#[ignore = "fixture invoked only inside the Docker acceptance container"]
fn docker_allocation_fixture() {
    let Some(marker) = std::env::var_os("RP_DOCKER_MARKER") else {
        return;
    };
    let mut allocation = vec![0_u8; ALLOCATION];
    for page in allocation.chunks_mut(4096) {
        page[0] = 7;
    }
    fs::write(marker, b"ready").unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        std::hint::black_box(&allocation);
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "fixture intentionally exceeds only the disposable container's memory cap"]
fn docker_outer_pressure_fixture() {
    if std::env::var_os("RP_DOCKER_DIRECTORY").is_none() {
        return;
    }
    assert_eq!(group(std::process::id()), "/broker");
    fs::write("/proc/self/oom_score_adj", "1000").unwrap();
    let mut allocation = vec![0_u8; 256 * 1024 * 1024];
    for page in allocation.chunks_mut(4096) {
        page[0] = 9;
    }
    std::hint::black_box(&allocation);
    panic!("outer container limit did not prevent a 256 MiB resident allocation");
}

#[test]
#[ignore = "requester fixture moved into the delegated worker cgroup"]
fn docker_worker_fixture() {
    let Some(directory) = std::env::var_os("RP_DOCKER_DIRECTORY") else {
        return;
    };
    let directory = PathBuf::from(directory);
    fs::write(
        format!("{CG}/worker/cgroup.procs"),
        std::process::id().to_string(),
    )
    .unwrap();
    assert_eq!(group(std::process::id()), "/worker");
    let baseline = memory("worker");
    let missing = directory.join("missing-broker");
    let missing_options = SpawnOptions {
        mode: SpawnMode::Independent,
        backend: Some(IndependentBackend::ExternalBroker {
            endpoint: missing.to_str().unwrap().to_owned(),
        }),
        ..SpawnOptions::default()
    };
    assert_eq!(
        spawn_with_options(
            &fixture("docker_allocation_fixture", &directory, None),
            &missing_options,
            &AtomicBool::new(false)
        )
        .err()
        .unwrap()
        .kind(),
        std::io::ErrorKind::Unsupported
    );
    assert!(!missing.exists());
    let inherited_ready = directory.join("inherited-ready");
    let mut spec = fixture(
        "docker_allocation_fixture",
        &directory,
        Some(&inherited_ready),
    );
    spec.environment
        .push(("RP_DOCKER_MARKER".into(), inherited_ready.into_os_string()));
    let inherited = start(&spec, &SpawnOptions::default());
    assert_eq!(group(inherited.id()), "/worker");
    let worker_before = memory("worker");
    assert!(worker_before >= baseline + 20 * 1024 * 1024);
    let broker_before = memory("broker");
    let independent_ready = directory.join("independent-ready");
    let mut spec = fixture(
        "docker_allocation_fixture",
        &directory,
        Some(&independent_ready),
    );
    spec.environment.push((
        "RP_DOCKER_MARKER".into(),
        independent_ready.into_os_string(),
    ));
    let options = SpawnOptions {
        mode: SpawnMode::Independent,
        lifetime: SpawnLifetime::Detached,
        backend: Some(IndependentBackend::ExternalBroker {
            endpoint: directory.join("broker/s").to_str().unwrap().to_owned(),
        }),
        timeout: Duration::from_secs(5),
    };
    let independent = start(&spec, &options);
    assert_eq!(independent.actual_mode(), SpawnMode::Independent);
    assert_eq!(group(independent.id()), "/broker");
    let worker_after = memory("worker");
    let broker_after = memory("broker");
    assert!(worker_after <= worker_before + 4 * 1024 * 1024);
    assert!(broker_after >= broker_before + 20 * 1024 * 1024);
    fs::write(directory.join("evidence"), format!("baseline={baseline}\nworker_before={worker_before}\nworker_after={worker_after}\nbroker_before={broker_before}\nbroker_after={broker_after}\n")).unwrap();
    let report = format!(
        "{} {} {} {}",
        inherited.id(),
        ticks(inherited.id()),
        independent.id(),
        ticks(independent.id())
    );
    drop(independent); // Detached: the broker retains ownership after this closes IPC.
    fs::write(directory.join("pending"), report).unwrap();
    fs::rename(directory.join("pending"), directory.join("complete")).unwrap();
    std::thread::sleep(Duration::from_secs(60));
    drop(inherited);
}

#[test]
#[ignore = "run only as PID 1 in the explicitly disposable private-cgroup Docker fixture"]
fn docker_broker_accounting() {
    assert_eq!(
        std::process::id(),
        1,
        "this fixture must not modify host cgroups"
    );
    assert_eq!(group(1), "/");
    assert_ne!(
        fs::read_to_string("/proc/1/comm").unwrap().trim(),
        "systemd"
    );
    assert!(!Path::new("/run/systemd/system").exists());
    assert_eq!(
        fs::read_to_string(format!("{CG}/memory.max"))
            .unwrap()
            .trim(),
        "134217728"
    );
    fs::create_dir(format!("{CG}/broker")).unwrap();
    fs::create_dir(format!("{CG}/worker")).unwrap();
    fs::write(format!("{CG}/broker/cgroup.procs"), "1").unwrap();
    fs::write(format!("{CG}/cgroup.subtree_control"), "+memory").unwrap();
    fs::write(format!("{CG}/worker/memory.max"), "67108864").unwrap();
    assert_eq!(
        fs::read_to_string(format!("{CG}/broker/memory.max"))
            .unwrap()
            .trim(),
        "max"
    );
    let directory = tempfile::tempdir().unwrap();
    let endpoint = directory.path().join("broker/s");
    let broker_log = directory.path().join("broker.log");
    let broker_spec = LaunchSpec {
        program: "/fixture/launcher".into(),
        args: vec!["--broker".into(), endpoint.as_os_str().to_owned()],
        cwd: directory.path().as_os_str().to_owned(),
        environment: vec![],
        stdout: Some(broker_log.as_os_str().to_owned()),
        stderr: Some(broker_log.as_os_str().to_owned()),
        readiness: Readiness::ProcessStarted,
    };
    let mut broker = start(&broker_spec, &SpawnOptions::default());
    wait_for("broker bind", || {
        assert!(
            broker.is_alive().unwrap(),
            "broker exited {:?}: {}",
            broker.wait(Duration::from_secs(1), &AtomicBool::new(false)),
            fs::read_to_string(&broker_log).unwrap_or_default()
        );
        endpoint.exists()
    });
    let worker_log = directory.path().join("worker.log");
    let mut worker_spec = fixture("docker_worker_fixture", directory.path(), None);
    worker_spec.stdout = Some(worker_log.as_os_str().to_owned());
    worker_spec.stderr = Some(worker_log.as_os_str().to_owned());
    let mut worker = start(&worker_spec, &SpawnOptions::default());
    wait_for("worker identity report", || {
        assert!(
            worker.is_alive().unwrap(),
            "worker failed before publishing identities: {}",
            fs::read_to_string(&worker_log).unwrap_or_default()
        );
        directory.path().join("complete").exists()
    });
    let report: Vec<u64> = fs::read_to_string(directory.path().join("complete"))
        .unwrap()
        .split_whitespace()
        .map(|value| value.parse().unwrap())
        .collect();
    assert_eq!(report.len(), 4);
    let inherited = Identity::pin(report[0] as u32, report[1]);
    let independent = Identity::pin(report[2] as u32, report[3]);
    fs::write(format!("{CG}/worker/cgroup.kill"), "1").unwrap();
    wait_for("worker teardown", || {
        !inherited.alive() && !worker.is_alive().unwrap()
    });
    for _ in 0..20 {
        assert!(independent.alive());
        std::thread::sleep(Duration::from_millis(10));
    }
    let oom_before = event("memory.events.local", "oom");
    let kills_before = event("memory.events", "oom_kill");
    let mut pressure = start(
        &fixture("docker_outer_pressure_fixture", directory.path(), None),
        &SpawnOptions::default(),
    );
    wait_for("outer memory pressure", || !pressure.is_alive().unwrap());
    assert!(
        event("memory.events.local", "oom") > oom_before,
        "outer cgroup did not enforce its limit"
    );
    assert!(event("memory.events", "oom_kill") > kills_before);
    // Independence excludes worker limits, not the container's outer limit.
    // The kernel may select this target as an outer-cgroup OOM victim.
    let survived_outer_oom = independent.alive();
    eprintln!(
        "{}survived_worker_teardown=true\nouter_memory_oom_enforced=true\nsurvived_outer_oom={survived_outer_oom}",
        fs::read_to_string(directory.path().join("evidence")).unwrap()
    );
    drop(independent);
    broker.stop(Duration::from_secs(5)).unwrap();
}
