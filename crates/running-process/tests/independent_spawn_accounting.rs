//! Real cgroup accounting and requester teardown evidence for #1202.
#![cfg(all(target_os = "linux", feature = "independent-spawn"))]

use running_process::independent_spawn::{spawn, IndependentChild, LaunchSpec, Readiness};
use std::{
    fs,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

const ALLOCATION: usize = 24 * 1024 * 1024;

fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < deadline, "fixture deadline expired");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn group(pid: u32) -> String {
    fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .unwrap()
        .to_owned()
}

fn memory(group: &str) -> u64 {
    fs::read_to_string(format!("/sys/fs/cgroup{group}/memory.current"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn control(command: &mut Command) {
    let mut child =
        running_process::spawn(command, running_process::SpawnStdio::default()).unwrap();
    let mut exit = None;
    wait_for(|| {
        exit = child.try_wait().unwrap();
        exit.is_some()
    });
    assert_eq!(exit, Some(0), "manager command failed");
}

struct Units {
    worker: Option<String>,
    directory: PathBuf,
}
impl Drop for Units {
    fn drop(&mut self) {
        let stop = |name: &str| {
            let mut command = Command::new("systemctl");
            command.args(["--user", "stop", name]);
            // Best effort even while unwinding; don't mask the test failure.
            if let Ok(mut child) =
                running_process::spawn(&mut command, running_process::SpawnStdio::default())
            {
                let deadline = Instant::now() + Duration::from_secs(3);
                while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        };
        // Stop publication before reading its final cleanup ownership record.
        if let Some(worker) = &self.worker {
            stop(worker);
        }
        if let Ok(name) = fs::read_to_string(self.directory.join("independent-unit")) {
            if name.starts_with("rp-independent-")
                && name.ends_with(".service")
                && !name.contains('/')
            {
                stop(&name);
            }
        }
    }
}

struct Pending(Option<IndependentChild>);
impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.stop(Duration::from_secs(2));
        }
    }
}

#[test]
#[ignore = "fixture invoked by the constrained worker"]
fn allocation_fixture() {
    let Some(marker) = std::env::var_os("RP_ACCOUNTING_MARKER") else {
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
#[ignore = "fixture launched in a memory-limited user service"]
fn accounting_worker() {
    let Some(directory) = std::env::var_os("RP_ACCOUNTING_DIRECTORY") else {
        return;
    };
    let directory = PathBuf::from(directory);
    let worker_group = group(std::process::id());
    let limit = fs::read_to_string(format!("/sys/fs/cgroup{worker_group}/memory.max")).unwrap();
    assert_eq!(limit.trim(), "134217728");
    let baseline = memory(&worker_group);
    let legacy_marker = directory.join("legacy-ready");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "allocation_fixture", "--ignored"])
        .env("RP_ACCOUNTING_MARKER", &legacy_marker);
    let mut legacy =
        running_process::spawn(&mut command, running_process::SpawnStdio::default()).unwrap();
    wait_for(|| legacy_marker.exists());
    assert_eq!(group(legacy.id()), worker_group);
    let legacy_memory = memory(&worker_group);
    assert!(legacy_memory.saturating_sub(baseline) >= ALLOCATION as u64 / 2);
    legacy.kill().unwrap();
    legacy.wait().unwrap();
    wait_for(|| memory(&worker_group) < baseline + ALLOCATION as u64 / 2);
    let before_independent = memory(&worker_group);
    let marker = directory.join("independent-ready");
    let spec = LaunchSpec {
        program: std::env::current_exe().unwrap().into_os_string(),
        args: vec![
            "--exact".into(),
            "allocation_fixture".into(),
            "--ignored".into(),
        ],
        cwd: directory.as_os_str().to_owned(),
        environment: vec![("RP_ACCOUNTING_MARKER".into(), marker.as_os_str().to_owned())],
        stdout: None,
        stderr: None,
        readiness: Readiness::File {
            path: marker.into_os_string(),
            value: b"ready".to_vec(),
        },
    };
    let mut child = Pending(Some(
        spawn(
            &spec,
            Path::new(env!("CARGO_BIN_EXE_running-process-launcher")),
            Duration::from_secs(10),
            &AtomicBool::new(false),
        )
        .unwrap(),
    ));
    let pid = child.0.as_ref().unwrap().id();
    let independent_group = group(pid);
    let unit = Path::new(&independent_group)
        .file_name()
        .unwrap()
        .to_str()
        .unwrap();
    // Publish cleanup ownership before assertions that can fail.
    fs::write(directory.join("independent-unit"), unit).unwrap();
    assert!(!independent_group.starts_with(&format!("{worker_group}/")));
    assert_ne!(independent_group, worker_group);
    let independent_memory = memory(&independent_group);
    let after_independent = memory(&worker_group);
    assert!(independent_memory >= ALLOCATION as u64);
    assert!(after_independent.saturating_sub(before_independent) < ALLOCATION as u64 / 2);
    fs::write(directory.join("pid"), pid.to_string()).unwrap();
    let evidence = format!("worker={worker_group}\nindependent={independent_group}\nbaseline={baseline}\ninherited={legacy_memory}\nworker_before={before_independent}\nworker_after={after_independent}\nindependent_memory={independent_memory}\n");
    fs::write(directory.join("evidence"), evidence).unwrap();
    drop(child.0.take()); // Explicitly detached after publishing cleanup ownership.
    fs::write(directory.join("complete"), b"complete").unwrap();
    std::thread::sleep(Duration::from_secs(60));
}

#[test]
#[ignore = "requires real systemd user services with cgroup-v2 memory accounting"]
fn independent_allocation_survives_requester_scope_teardown() {
    let directory = tempfile::tempdir().unwrap();
    let unit = format!(
        "rp-accounting-{}.service",
        directory.path().file_name().unwrap().to_str().unwrap()
    );
    let mut cleanup = Units {
        worker: Some(unit.clone()),
        directory: directory.path().to_owned(),
    };
    let mut command = Command::new("systemd-run");
    command.args([
        "--user",
        "--quiet",
        "--collect",
        "--service-type=exec",
        "--property=MemoryMax=128M",
        "--property=MemoryAccounting=yes",
        "--property=TimeoutStopSec=1s",
        "--unit",
        &unit,
    ]);
    command.arg(format!(
        "--setenv=RP_ACCOUNTING_DIRECTORY={}",
        directory.path().display()
    ));
    command
        .arg("--")
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "accounting_worker", "--ignored", "--nocapture"]);
    control(&mut command);
    wait_for(|| directory.path().join("complete").exists());
    let pid: u32 = fs::read_to_string(directory.path().join("pid"))
        .unwrap()
        .parse()
        .unwrap();
    // SAFETY: pidfd_open takes a PID and flags; success transfers a new fd.
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(
        descriptor >= 0,
        "pidfd_open: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: descriptor is a newly returned, uniquely owned pidfd.
    let identity = unsafe { OwnedFd::from_raw_fd(descriptor as i32) };
    let mut stop = Command::new("systemctl");
    stop.args(["--user", "stop", &unit]);
    control(&mut stop);
    cleanup.worker = None;
    let mut event = libc::pollfd {
        fd: identity.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: event points to one initialized pollfd with a borrowed live fd.
    assert_eq!(
        unsafe { libc::poll(&mut event, 1, 0) },
        0,
        "pinned daemon exited with worker"
    );
    eprintln!(
        "{}survived_requester_teardown=true",
        fs::read_to_string(directory.path().join("evidence")).unwrap()
    );
    drop(cleanup);
    wait_for(|| {
        // SAFETY: the same live pidfd and initialized pollfd remain owned here.
        (unsafe { libc::poll(&mut event, 1, 0) }) == 1
    });
}
