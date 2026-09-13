//! Explicitly enabled Windows Task Scheduler integration.
#![cfg(all(target_os = "windows", feature = "independent-spawn"))]

use running_process::independent_spawn::{spawn, IndependentChild, LaunchSpec, Readiness};
use std::{fs, path::Path, sync::atomic::AtomicBool, time::Duration};

struct Cleanup(IndependentChild);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.0.stop(Duration::from_secs(5));
    }
}

#[test]
#[ignore = "helper fixture, invoked only by the scheduler integration test"]
fn target_fixture() {
    let Some(directory) = std::env::var_os("RP_INDEPENDENT_TEST_DIRECTORY") else {
        return;
    };
    fs::write(Path::new(&directory).join("ready"), b"ready").unwrap();
    std::thread::sleep(Duration::from_secs(60));
}

#[test]
#[ignore = "requires same-user Task Scheduler access and an interactive user session"]
fn scheduler_starts_and_stops_the_verified_target() {
    let directory = tempfile::tempdir().unwrap();
    let spec = LaunchSpec {
        program: std::env::current_exe().unwrap().into_os_string(),
        args: vec![
            "--exact".into(),
            "target_fixture".into(),
            "--ignored".into(),
        ],
        cwd: directory.path().as_os_str().to_owned(),
        environment: vec![
            ("SystemRoot".into(), std::env::var_os("SystemRoot").unwrap()),
            (
                "RP_INDEPENDENT_TEST_DIRECTORY".into(),
                directory.path().as_os_str().to_owned(),
            ),
        ],
        stdout: None,
        stderr: None,
        readiness: Readiness::File {
            path: directory.path().join("ready").into_os_string(),
            value: b"ready".to_vec(),
        },
    };
    let mut child = Cleanup(
        spawn(
            &spec,
            Path::new(env!("CARGO_BIN_EXE_running-process-launcher")),
            Duration::from_secs(25),
            &AtomicBool::new(false),
        )
        .unwrap(),
    );
    assert!(child.0.is_alive());
    assert_eq!(fs::read(directory.path().join("ready")).unwrap(), b"ready");
    child.0.stop(Duration::from_secs(5)).unwrap();
    assert!(!child.0.is_alive());
}
