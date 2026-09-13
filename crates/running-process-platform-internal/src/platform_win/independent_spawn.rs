//! Task Scheduler-owned helper and target handshake with Job Object evidence.
use super::{
    process_inspect::{process_executable_path, process_same_executable_path, ProcessLiveness},
    scheduler_launch::ScheduledTask,
};
use crate::platform::{
    independent_spawn::{check, is_ready, receive, send, Channel, LaunchSpec, Message, LEASE},
    ipc::{current_user_id, Endpoint, Listener, ListenerNonblockingMode},
};
use std::{
    io,
    path::Path,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

/// Detached, handle-pinned target and supervisor. Dropping preserves lifetime.
pub struct IndependentChild {
    process: ProcessLiveness,
    helper: ProcessLiveness,
}
impl IndependentChild {
    pub fn id(&self) -> u32 {
        self.process.pid()
    }
    pub fn is_alive(&self) -> bool {
        self.process.is_alive()
    }
    pub fn stop(&mut self, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        self.process.terminate_pinned()?;
        self.helper.terminate_pinned()?;
        while self.process.is_alive() || self.helper.is_alive() {
            check(deadline, &AtomicBool::new(false))?;
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }
    pub fn wait(&self, timeout: Duration, cancelled: &AtomicBool) -> io::Result<()> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        while self.process.is_alive() {
            check(deadline, cancelled)?;
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }
}

struct Pending(Option<ProcessLiveness>);
impl Pending {
    fn get(&self) -> &ProcessLiveness {
        self.0.as_ref().expect("pending process owned")
    }
    fn commit(mut self) -> ProcessLiveness {
        self.0.take().expect("pending process owned")
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(process) = &self.0 {
            let _ = process.terminate_pinned();
        }
    }
}

pub fn spawn(
    spec: &LaunchSpec,
    helper_path: &Path,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> io::Result<IndependentChild> {
    if timeout.is_zero() || timeout > LEASE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "launch timeout must fit the 30-second helper lease",
        ));
    }
    let deadline = Instant::now() + timeout;
    check(deadline, cancelled)?;
    spec.validate()?;
    send(
        &mut io::sink(),
        &Message::Launch(spec.clone()),
        deadline,
        cancelled,
    )?;
    let (mut task, address) = ScheduledTask::prepare(helper_path)?;
    let endpoint = Endpoint::new(address)?;
    let listener = Listener::bind_owner_only(&endpoint)?;
    listener.set_nonblocking(ListenerNonblockingMode::Both)?;
    task.start(deadline, cancelled)?;
    let stream = loop {
        check(deadline, cancelled)?;
        match listener.accept() {
            Ok(stream) => break stream,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                std::thread::sleep(Duration::from_millis(2))
            }
            Err(error) => return Err(error),
        }
    };
    let peer = stream.peer_identity()?;
    if peer.pid == 0 || peer.user_id != current_user_id()? {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    // Pin identity first, but do not arm termination for an unauthenticated
    // peer. A same-user process connecting to the pipe is not ours to kill.
    let helper = ProcessLiveness::open_pinned(peer.pid)?;
    if !process_same_executable_path(&process_executable_path(peer.pid)?, helper_path) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unexpected launcher executable",
        ));
    }
    if !helper.is_alive() {
        return Err(io::Error::from(io::ErrorKind::BrokenPipe));
    }
    let helper = Pending(Some(helper));
    if !helper.get().outside_current_job(deadline)? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "scheduler helper retained caller Job Object",
        ));
    }
    let mut stream = Channel::new(stream)?;
    send(
        &mut stream,
        &Message::Launch(spec.clone()),
        deadline,
        cancelled,
    )
    .map_err(|error| io::Error::new(error.kind(), "target payload transfer failed"))?;
    let pid = match receive(&mut stream, deadline, cancelled)
        .map_err(|error| io::Error::new(error.kind(), "target identity response failed"))?
    {
        Message::Started { pid } => pid,
        Message::Failed { kind } => return Err(kind.into_io()),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected target identity",
            ))
        }
    };
    let process = Pending(Some(ProcessLiveness::open_pinned(pid)?));
    if !process.get().outside_current_job(deadline)? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "target retained caller Job Object",
        ));
    }
    while !is_ready(&spec.readiness)? {
        check(deadline, cancelled)?;
        if !process.get().is_alive() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "target exited before readiness",
            ));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    if !process.get().outside_current_job(deadline)? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "target changed Job Object during startup",
        ));
    }
    send(&mut stream, &Message::Commit, deadline, cancelled)?;
    if !matches!(
        receive(&mut stream, deadline, cancelled)?,
        Message::Committed
    ) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected commit acknowledgement",
        ));
    }
    // Removing a definition leaves its running instance alive. Retain pinned
    // rollback ownership until that removal and the final cancellation check.
    task.remove_definition(deadline, cancelled)?;
    check(deadline, cancelled)?;
    Ok(IndependentChild {
        process: process.commit(),
        helper: helper.commit(),
    })
}

#[cfg(test)]
mod job_tests {
    use super::*;
    use crate::platform::independent_spawn::Readiness;
    use std::{
        fs,
        os::windows::io::{FromRawHandle, OwnedHandle},
    };
    use windows_sys::Win32::System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Threading::GetCurrentProcess,
    };

    struct Cleanup(IndependentChild);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.stop(Duration::from_secs(5));
        }
    }

    struct Worker {
        child: crate::platform::independent_spawn::InheritedChild,
        abort: std::path::PathBuf,
    }
    impl Drop for Worker {
        fn drop(&mut self) {
            // Give the worker a chance to clean up after a failed identity handoff.
            let _ = fs::write(&self.abort, b"abort");
            let _ = self
                .child
                .wait(Duration::from_secs(6), &AtomicBool::new(false));
            let _ = self.child.stop(Duration::from_secs(5));
        }
    }

    #[test]
    #[ignore = "target fixture invoked only by restrictive_job_scheduler_separation"]
    fn target_fixture() {
        let directory = std::env::var_os("RP_JOB_TARGET_DIRECTORY").expect("fixture directory");
        fs::write(Path::new(&directory).join("ready"), b"ready").unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    #[test]
    #[ignore = "must run alone in a dedicated test process with Task Scheduler access"]
    fn restrictive_job_scheduler_separation() {
        let helper = std::env::var_os("RP_INDEPENDENT_LAUNCHER").expect("launcher path");
        assert!(Path::new(&helper).is_file());
        // SAFETY: null name/security selects a private unnamed Job Object.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        assert!(!raw.is_null(), "{}", io::Error::last_os_error());
        // SAFETY: CreateJobObjectW returned a new, exclusively owned handle.
        let job = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
        // SAFETY: zero is the documented empty limit structure.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_JOB_MEMORY;
        limits.JobMemoryLimit = 256 * 1024 * 1024;
        // No BREAKAWAY_OK or SILENT_BREAKAWAY_OK flag is permitted.
        // SAFETY: raw remains owned and the structure/size match the information class.
        assert_ne!(
            unsafe {
                SetInformationJobObject(
                    raw,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32,
                )
            },
            0,
            "{}",
            io::Error::last_os_error()
        );
        // SAFETY: both handles are valid; only this dedicated test process is assigned.
        assert_ne!(
            unsafe { AssignProcessToJobObject(raw, GetCurrentProcess()) },
            0,
            "{}",
            io::Error::last_os_error()
        );
        // Closing the last handle would kill this test harness before it reports
        // its result. The kernel closes it at process exit; never run this fixture
        // alongside unrelated tests in the same process.
        std::mem::forget(job);

        // Abrupt requester teardown bypasses TempDir::drop. Put its scratch
        // directory under the controller's directory so the controller owns
        // cleanup even when the worker is killed.
        let directory = match std::env::var_os("RP_JOB_REPORT_FILE") {
            Some(report) => tempfile::tempdir_in(Path::new(&report).parent().unwrap()).unwrap(),
            None => tempfile::tempdir().unwrap(),
        };
        let spec = LaunchSpec {
            program: std::env::current_exe().unwrap().into_os_string(),
            args: vec![
                "--exact".into(),
                "platform_win::independent_spawn::job_tests::target_fixture".into(),
                "--ignored".into(),
            ],
            cwd: directory.path().as_os_str().to_owned(),
            environment: vec![
                ("SystemRoot".into(), std::env::var_os("SystemRoot").unwrap()),
                (
                    "RP_JOB_TARGET_DIRECTORY".into(),
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
                Path::new(&helper),
                Duration::from_secs(25),
                &AtomicBool::new(false),
            )
            .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let outside = child.0.process.outside_current_job(deadline);
        // Use the exact job, independently of the production subtree verifier.
        // SAFETY: this only opens a query handle; the original pinned identity
        // is checked alive after the query, so PID reuse cannot pass the test.
        let queried = unsafe {
            windows_sys::Win32::System::Threading::OpenProcess(
                windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                child.0.id(),
            )
        };
        assert!(!queried.is_null(), "{}", io::Error::last_os_error());
        // SAFETY: OpenProcess returned a fresh, exclusively owned handle.
        let _queried_owner = unsafe { OwnedHandle::from_raw_handle(queried.cast()) };
        let mut in_restrictive_job = 0;
        // SAFETY: both handles are live and the BOOL output is writable.
        assert_ne!(
            unsafe {
                windows_sys::Win32::System::JobObjects::IsProcessInJob(
                    queried,
                    raw,
                    &mut in_restrictive_job,
                )
            },
            0,
            "{}",
            io::Error::last_os_error()
        );
        let alive = child.0.is_alive();
        if let Some(report) = std::env::var_os("RP_JOB_REPORT_FILE") {
            assert!(outside.unwrap());
            assert!(alive);
            assert_eq!(in_restrictive_job, 0);
            let identities = [
                (
                    child.0.process.pid(),
                    child.0.process.test_creation_time().unwrap(),
                ),
                (
                    child.0.helper.pid(),
                    child.0.helper.test_creation_time().unwrap(),
                ),
            ];
            let pending = Path::new(&report).with_extension("pending");
            fs::write(&pending, serde_json::to_vec(&identities).unwrap()).unwrap();
            fs::rename(pending, &report).unwrap();
            // The controller pins both identities before killing this worker's
            // containing job. Abrupt worker death must not run this Cleanup.
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline {
                if Path::new(&report).with_extension("abort").is_file() {
                    return; // Cleanup stops the pair after a failed handoff.
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("controller failed to terminate the requester job");
        }
        child.0.stop(Duration::from_secs(5)).unwrap();
        assert!(
            outside.unwrap(),
            "scheduled target remained in the restrictive job"
        );
        assert!(alive);
        assert_eq!(
            in_restrictive_job, 0,
            "target is a member of the exact restrictive job"
        );
        assert!(!child.0.is_alive());
    }

    #[test]
    #[ignore = "requires a real Windows scheduler and a separate requester test process"]
    fn requester_teardown_preserves_independent_target() {
        use crate::platform::independent_spawn::spawn_inherited;
        let helper = std::env::var_os("RP_INDEPENDENT_LAUNCHER").expect("launcher path");
        let directory = tempfile::tempdir().unwrap();
        let report = directory.path().join("identities.json");
        let log = directory.path().join("worker.log");
        let spec = LaunchSpec {
            program: std::env::current_exe().unwrap().into_os_string(),
            args: vec![
                "--exact".into(),
                "platform_win::independent_spawn::job_tests::restrictive_job_scheduler_separation"
                    .into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
            cwd: directory.path().as_os_str().to_owned(),
            environment: vec![
                ("SystemRoot".into(), std::env::var_os("SystemRoot").unwrap()),
                ("RP_INDEPENDENT_LAUNCHER".into(), helper),
                ("RP_JOB_REPORT_FILE".into(), report.as_os_str().to_owned()),
            ],
            stdout: Some(log.as_os_str().to_owned()),
            stderr: Some(log.as_os_str().to_owned()),
            readiness: Readiness::ProcessStarted,
        };
        let cancelled = AtomicBool::new(false);
        let mut worker = Worker {
            child: spawn_inherited(&spec, false, Duration::from_secs(5), &cancelled).unwrap(),
            abort: report.with_extension("abort"),
        };
        let deadline = Instant::now() + Duration::from_secs(40);
        while !report.is_file() {
            if worker.child.try_wait().unwrap().is_some() || Instant::now() >= deadline {
                panic!(
                    "worker did not report target identities: {}",
                    fs::read_to_string(&log).unwrap_or_default()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let identities: [(u32, u64); 2] =
            serde_json::from_slice(&fs::read(report).unwrap()).unwrap();
        let pin = |(pid, created)| {
            let process = ProcessLiveness::open_pinned(pid).unwrap();
            assert_eq!(
                process.test_creation_time().unwrap(),
                created,
                "reported process identity was reused before controller pinning"
            );
            assert!(process.is_alive());
            process
        };
        let mut child = Cleanup(IndependentChild {
            process: pin(identities[0]),
            helper: pin(identities[1]),
        });
        // InheritedChild::stop terminates the owned containing Job Object,
        // including the requester's nested restrictive job, not only its PID.
        worker.child.stop(Duration::from_secs(5)).unwrap();
        assert!(worker.child.try_wait().unwrap().is_some());
        for _ in 0..20 {
            assert!(
                child.0.is_alive(),
                "independent target died with requester job"
            );
            assert!(
                child.0.helper.is_alive(),
                "independent helper died with requester job"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        child.0.stop(Duration::from_secs(5)).unwrap();
        assert!(!child.0.is_alive());
        assert!(!child.0.helper.is_alive());
    }
}
