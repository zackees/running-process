//! Opt-in Task Scheduler acceptance. These tests perform real launches and
//! require prebuilt fixtures and an accessible scheduler under the test user.
//! Caller-exit evidence is distinct from Job teardown and memory accounting.
#![cfg(windows)]

use running_process::{
    spawn_daemon_request, DaemonSpawnRequest, IndependentSpawnOptions, SpawnMode,
};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

struct RequesterJob(std::os::windows::io::OwnedHandle);

impl RequesterJob {
    fn assign(pid: u32) -> io::Result<Self> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };
        // SAFETY: unnamed Job, default security, no borrowed pointer inputs.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful creation transfers a new owned handle.
        let job = Self(unsafe { OwnedHandle::from_raw_handle(raw) });
        // Only the newly created, gated fixture PID is supplied by the caller.
        let raw_process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if raw_process.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful OpenProcess returns a new owned handle.
        let process = unsafe { OwnedHandle::from_raw_handle(raw_process) };
        // SAFETY: both handles are owned and live for the complete operation.
        if unsafe { AssignProcessToJobObject(job.0.as_raw_handle(), process.as_raw_handle()) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    fn terminate(&self) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: this handle names only the unnamed Job created by this test.
        if unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0.as_raw_handle(), 137)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for RequesterJob {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    launcher: Option<running_process::DaemonChild>,
    daemon: Option<running_process_platform_internal::ProcessLiveness>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Cooperative daemon cancellation also works before allocation. Never
        // terminate an arbitrary PID parsed from a marker during panic cleanup.
        let _ = std::fs::write(self.directory.path().join("release"), b"release");
        let _ = std::fs::write(self.directory.path().join("exit-launcher"), b"exit");
        if let Some(daemon) = self.daemon.as_ref() {
            if !matches!(daemon.has_exited(), Ok(true)) {
                // This is the already-held kernel process object, never a
                // fresh lookup of a possibly recycled marker PID.
                let _ = daemon.kill();
                let deadline = Instant::now() + Duration::from_secs(3);
                while Instant::now() < deadline {
                    if matches!(daemon.has_exited(), Ok(true)) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                if !matches!(daemon.has_exited(), Ok(true)) {
                    eprintln!("daemon termination unconfirmed through held process handle");
                }
            }
        }
        if let Some(launcher) = self.launcher.as_mut() {
            if !matches!(launcher.try_wait(), Ok(Some(_))) {
                let _ = launcher.kill();
                let deadline = Instant::now() + Duration::from_secs(3);
                while Instant::now() < deadline {
                    if matches!(launcher.try_wait(), Ok(Some(_))) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                eprintln!("launcher cleanup unconfirmed; fixture has finite internal deadlines");
            }
        }
    }
}

fn marker(directory: &Path, name: &str) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        if let Ok(failure) = std::fs::read_to_string(directory.join("launcher-failed")) {
            panic!(
                "scheduler launch failed with fixture category {}",
                failure.trim()
            );
        }
        match std::fs::read_to_string(directory.join(name)) {
            // Marker creation and writing are separate syscalls. Wait for a
            // complete newline-terminated value, not just path existence.
            Ok(text) if text.ends_with('\n') => {
                return text.trim().parse().expect("numeric marker")
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => panic!("cannot read fixture marker: {error}"),
        }
        assert!(Instant::now() < deadline, "fixture marker deadline: {name}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn scheduler_daemon_performs_new_work_after_launcher_exit() {
    run_caller_lifetime_case(false, SpawnMode::Independent);
}

#[test]
fn scheduler_daemon_performs_new_work_after_owned_job_teardown() {
    run_caller_lifetime_case(true, SpawnMode::Independent);
}

#[test]
fn inherited_daemon_dies_with_owned_requester_job() {
    run_caller_lifetime_case(true, SpawnMode::Inherited);
}

fn run_caller_lifetime_case(terminate_job: bool, daemon_mode: SpawnMode) {
    if std::env::var("RUNNING_PROCESS_LIVE_TESTS").as_deref() != Ok("1")
        || std::env::var("RUNNING_PROCESS_INDEPENDENT_WINDOWS_TESTS").as_deref() != Ok("1")
    {
        eprintln!("skipping opt-in Windows scheduler acceptance");
        return;
    }
    let profile = std::env::current_exe()
        .expect("test image")
        .parent()
        .expect("deps directory")
        .parent()
        .expect("profile directory")
        .to_path_buf();
    let launcher_path = profile.join("testbin-independent-launcher.exe");
    assert!(
        launcher_path.is_file(),
        "build matching testbins before acceptance"
    );
    assert!(profile
        .join("testbin-independent-memory-holder.exe")
        .is_file());
    assert!(
        profile
            .join("running-process-independent-helper.exe")
            .is_file(),
        "build the independent scheduler helper beside the launcher fixture"
    );
    let mut fixture = Fixture {
        directory: tempfile::tempdir().expect("private fixture directory"),
        launcher: None,
        daemon: None,
    };
    let mut request = DaemonSpawnRequest::new(launcher_path);
    request
        .arg(fixture.directory.path())
        .arg(match daemon_mode {
            SpawnMode::Inherited => "inherited",
            SpawnMode::Independent => "independent",
        });
    fixture.launcher = Some(
        spawn_daemon_request(
            &mut request,
            &IndependentSpawnOptions {
                mode: SpawnMode::Inherited,
                ..Default::default()
            },
        )
        .expect("inherited launcher"),
    );
    let directory = fixture.directory.path();
    assert_eq!(
        marker(directory, "launcher-started"),
        fixture.launcher.as_ref().unwrap().id()
    );
    // The launcher is still waiting for the launch marker: assigning it now
    // ensures its independent-spawn request originates inside this fresh Job.
    let job = terminate_job.then(|| {
        RequesterJob::assign(fixture.launcher.as_ref().unwrap().id())
            .expect("assign gated launcher to fixture-owned Job")
    });
    std::fs::write(directory.join("launch"), b"launch").unwrap();
    let daemon_pid = marker(directory, "launched");
    assert_eq!(marker(directory, "started"), daemon_pid);
    assert_ne!(daemon_pid, fixture.launcher.as_ref().unwrap().id());
    fixture.daemon = Some(
        running_process_platform_internal::ProcessLiveness::open_for_control(daemon_pid)
            .expect("hold exact daemon process object"),
    );
    let daemon = fixture.daemon.as_ref().unwrap();
    let created = daemon.creation_time().expect("daemon creation FILETIME");
    assert!(!daemon
        .has_exited()
        .expect("daemon liveness before caller exit"));
    if let Some(job) = job.as_ref() {
        job.terminate().expect("terminate only the requester Job");
    } else {
        std::fs::write(directory.join("exit-launcher"), b"exit").unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = fixture
            .launcher
            .as_mut()
            .unwrap()
            .try_wait()
            .expect("launcher status")
        {
            assert_eq!(status, if terminate_job { 137 } else { 0 });
            break;
        }
        assert!(Instant::now() < deadline, "launcher did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Request work only after the launcher is confirmed exited. A startup
    // marker alone cannot establish independent lifetime.
    if daemon_mode == SpawnMode::Inherited {
        assert!(
            terminate_job,
            "negative control requires owned Job teardown"
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !daemon
            .has_exited()
            .expect("inherited daemon terminal state")
        {
            assert!(
                Instant::now() < deadline,
                "inherited daemon escaped the requester Job"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !directory.join("resident").exists(),
            "negative control must not allocate"
        );
        return;
    }
    assert!(!daemon
        .has_exited()
        .expect("same daemon survives caller exit"));
    assert_eq!(daemon.creation_time().unwrap(), created);
    std::fs::write(directory.join("allocate"), b"allocate").unwrap();
    assert_eq!(marker(directory, "resident"), 64 * 1024 * 1024);
    std::fs::write(directory.join("release"), b"release").unwrap();
    assert_eq!(marker(directory, "released"), daemon_pid);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !daemon.has_exited().expect("daemon terminal status") {
        assert!(
            Instant::now() < deadline,
            "daemon did not exit after release"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
