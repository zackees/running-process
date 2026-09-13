//! Real Linux scheduler/helper contract; explicitly opt in on systemd hosts.
#![cfg(all(target_os = "linux", feature = "independent-spawn"))]

use running_process::independent_spawn::{spawn, IndependentChild, LaunchSpec};
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
