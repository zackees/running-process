//! Real Linux scheduler/helper contract; explicitly opt in on systemd hosts.
#![cfg(all(target_os = "linux", feature = "independent-spawn"))]

use running_process::independent_spawn::{spawn, IndependentChild, LaunchSpec, Readiness};
use std::{
    ffi::OsString,
    fs,
    os::unix::ffi::OsStringExt,
    path::Path,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

struct Cleanup(IndependentChild);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.0.stop(Duration::from_secs(2));
    }
}

fn sleep_spec(directory: &Path) -> LaunchSpec {
    LaunchSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "exec sleep 60".into()],
        cwd: directory.as_os_str().to_owned(),
        environment: vec![("PATH".into(), std::env::var_os("PATH").unwrap())],
        stdout: None,
        stderr: None,
        readiness: Readiness::ProcessStarted,
    }
}

#[test]
fn preexisting_readiness_is_not_accepted_as_a_new_daemon() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("ready");
    fs::write(&marker, b"ready").unwrap();
    let mut spec = sleep_spec(directory.path());
    spec.readiness = Readiness::File {
        path: marker.into_os_string(),
        value: b"ready".to_vec(),
    };
    let result = spawn(
        &spec,
        Path::new("/absent/helper"),
        Duration::from_secs(1),
        &AtomicBool::new(false),
    );
    assert_eq!(
        result.err().unwrap().kind(),
        std::io::ErrorKind::AlreadyExists
    );
}

#[test]
#[ignore = "requires accessible systemd user manager, cgroup v2 and pidfds"]
fn application_readiness_precedes_commit() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("ready");
    let mut spec = sleep_spec(directory.path());
    spec.args = vec![
        "-c".into(),
        "sleep 0.05; printf ready > \"$1\"; exec sleep 60".into(),
        "fixture".into(),
        marker.as_os_str().to_owned(),
    ];
    spec.readiness = Readiness::File {
        path: marker.as_os_str().to_owned(),
        value: b"ready".to_vec(),
    };
    let mut child = Cleanup(
        spawn(
            &spec,
            Path::new(env!("CARGO_BIN_EXE_running-process-launcher")),
            Duration::from_secs(5),
            &AtomicBool::new(false),
        )
        .unwrap(),
    );
    assert_eq!(fs::read(marker).unwrap(), b"ready");
    assert!(child.0.is_alive());
    child.0.stop(Duration::from_secs(2)).unwrap();
}

#[test]
#[ignore = "requires accessible systemd user manager, cgroup v2 and pidfds"]
fn readiness_timeout_rolls_back_started_target() {
    let directory = tempfile::tempdir().unwrap();
    let pidfile = directory.path().join("pid");
    let mut spec = sleep_spec(directory.path());
    spec.args = vec![
        "-c".into(),
        "printf '%s' $$ > \"$1\"; exec sleep 60".into(),
        "fixture".into(),
        pidfile.as_os_str().to_owned(),
    ];
    spec.readiness = Readiness::File {
        path: directory.path().join("never-ready").into_os_string(),
        value: b"ready".to_vec(),
    };
    let result = spawn(
        &spec,
        Path::new(env!("CARGO_BIN_EXE_running-process-launcher")),
        Duration::from_millis(500),
        &AtomicBool::new(false),
    );
    assert_eq!(result.err().unwrap().kind(), std::io::ErrorKind::TimedOut);
    let pid: u32 = fs::read_to_string(pidfile).unwrap().parse().unwrap();
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "timed-out target survived rollback"
    );
}

#[test]
#[ignore = "requires accessible systemd user manager, cgroup v2 and pidfds"]
fn symlinked_log_is_rejected_without_writing_through_it() {
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("existing");
    fs::write(&destination, b"untouched").unwrap();
    let link = directory.path().join("log");
    std::os::unix::fs::symlink(&destination, &link).unwrap();
    let mut spec = sleep_spec(directory.path());
    spec.stdout = Some(link.into_os_string());
    let result = spawn(
        &spec,
        Path::new(env!("CARGO_BIN_EXE_running-process-launcher")),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    );
    assert_eq!(
        result.err().unwrap().kind(),
        std::io::ErrorKind::Unsupported
    );
    assert_eq!(fs::read(destination).unwrap(), b"untouched");
}

#[test]
#[ignore = "requires accessible systemd user manager, cgroup v2 and pidfds"]
fn scheduler_helper_preserves_native_payload_and_controls_actual_target() {
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("stdout");
    let opaque = OsString::from_vec(b"a \xff$;*".to_vec());
    let spec = LaunchSpec {
        program: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "printf '%s\n' \"$1\" \"$2\" \"$TOKEN\"; pwd; exec sleep 60".into(),
            "fixture".into(),
            "".into(),
            opaque,
        ],
        cwd: directory.path().as_os_str().to_owned(),
        environment: vec![
            ("PATH".into(), std::env::var_os("PATH").unwrap()),
            ("TOKEN".into(), "literal $value;*".into()),
        ],
        stdout: Some(log.as_os_str().to_owned()),
        stderr: None,
        readiness: Readiness::ProcessStarted,
    };
    let mut child = Cleanup(
        spawn(
            &spec,
            Path::new(env!("CARGO_BIN_EXE_running-process-launcher")),
            Duration::from_secs(10),
            &AtomicBool::new(false),
        )
        .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let expected = [
        b"\na \xff$;*\nliteral $value;*\n".as_slice(),
        directory.path().as_os_str().as_encoded_bytes(),
        b"\n",
    ]
    .concat();
    loop {
        if fs::read(&log).unwrap_or_default() == expected {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "target output did not preserve the launch payload"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(child.0.is_alive());
    assert_eq!(
        child
            .0
            .wait(Duration::from_millis(20), &AtomicBool::new(false))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::TimedOut
    );
    child.0.stop(Duration::from_secs(3)).unwrap();
    child
        .0
        .wait(Duration::from_secs(1), &AtomicBool::new(false))
        .unwrap();
    assert!(!child.0.is_alive());
}

#[test]
fn cancellation_prevents_scheduler_side_effects() {
    let spec = LaunchSpec {
        program: "/absent/program".into(),
        args: vec![],
        cwd: "/".into(),
        environment: vec![],
        stdout: None,
        stderr: None,
        readiness: Readiness::ProcessStarted,
    };
    let result = spawn(
        &spec,
        Path::new("/absent/helper"),
        Duration::from_secs(1),
        &AtomicBool::new(true),
    );
    assert_eq!(
        result.err().unwrap().kind(),
        std::io::ErrorKind::Interrupted
    );
}

#[test]
#[ignore = "requires accessible systemd user manager, cgroup v2 and pidfds"]
fn target_exec_failure_is_reported_without_a_daemon() {
    let directory = tempfile::tempdir().unwrap();
    let spec = LaunchSpec {
        program: directory.path().join("absent-program").into_os_string(),
        args: vec![],
        cwd: directory.path().as_os_str().to_owned(),
        environment: vec![],
        stdout: None,
        stderr: None,
        readiness: Readiness::ProcessStarted,
    };
    let result = spawn(
        &spec,
        Path::new(env!("CARGO_BIN_EXE_running-process-launcher")),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    );
    assert_eq!(result.err().unwrap().kind(), std::io::ErrorKind::NotFound);
}

#[test]
#[ignore = "requires accessible systemd user manager, cgroup v2 and pidfds"]
fn cancellation_during_helper_handshake_is_bounded() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{atomic::Ordering, Arc};
    let directory = tempfile::tempdir().unwrap();
    let helper = directory.path().join("unresponsive-helper");
    fs::write(&helper, "#!/bin/sh\nexec sleep 60\n").unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    let spec = LaunchSpec {
        program: "/bin/sh".into(),
        args: vec![],
        cwd: "/".into(),
        environment: vec![],
        stdout: None,
        stderr: None,
        readiness: Readiness::ProcessStarted,
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();
    let cancel = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        flag.store(true, Ordering::Release);
    });
    let start = Instant::now();
    let result = spawn(&spec, &helper, Duration::from_secs(5), &cancelled);
    cancel.join().unwrap();
    assert_eq!(
        result.err().unwrap().kind(),
        std::io::ErrorKind::Interrupted
    );
    assert!(start.elapsed() < Duration::from_secs(3));
}
