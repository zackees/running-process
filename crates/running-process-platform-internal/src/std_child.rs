//! The non-Tokio child backend (#850).
//!
//! [`PlatformStdChild`] owns a child spawned by a synchronous launch path: a
//! plain [`std::process::Child`], or a [`TracedChild`] whose exit is reaped by
//! its dedicated exact-trace tracer (Tokio cannot adopt either). Exit is
//! observed by polling [`PlatformStdChild::try_wait_code`], which is what lets
//! a timer task on the shared actor runtime own the child without Tokio's
//! `process` feature and without an I/O driver: a process-only build compiles
//! this module with no Tokio at all.
//!
//! It also carries the per-spawn containment the synchronous engine attaches
//! after launch -- the Windows Job Object (descendant observer and memory
//! limit) -- and the capture-cancellation wiring for its output pipes, so the
//! owner of the child owns everything whose lifetime is tied to it.

use std::io::{self, Read};
use std::time::Duration;

use crate::platform::process::{CaptureCancellation, CaptureStream, UnixSignalKind};
use crate::{
    capture_reader_done, exit_code, prepare_capture_reader, unix_signal_process_group, TracedChild,
    WindowsJobHandle,
};

/// Interval of the blocking exact-trace wait fallback.
const TRACED_WAIT_POLL: Duration = Duration::from_millis(10);

enum StdChildKind {
    Standard(std::process::Child),
    ExactTrace(TracedChild),
}

/// A synchronously launched child whose exit is observed by polling.
pub struct PlatformStdChild {
    // Field order is drop order: the child handle is released before the Job
    // Object whose kill-on-close contains it, as the engine always did.
    kind: StdChildKind,
    job: Option<WindowsJobHandle>,
    own_process_group: bool,
}

/// The two prepared, cancellable capture readers of a child.
pub struct PlatformCaptureReaders {
    pub stdout: Box<dyn Read + Send>,
    pub stderr: Box<dyn Read + Send>,
}

impl PlatformStdChild {
    /// Adopt a standard child. `own_process_group` records whether it was
    /// launched as the leader of its own process group.
    pub fn from_std(child: std::process::Child, own_process_group: bool) -> Self {
        Self::new(StdChildKind::Standard(child), own_process_group)
    }

    /// Adopt an exact-trace child; its tracer remains the sole reaper.
    pub fn from_exact_trace(child: TracedChild, own_process_group: bool) -> Self {
        Self::new(StdChildKind::ExactTrace(child), own_process_group)
    }

    fn new(kind: StdChildKind, own_process_group: bool) -> Self {
        Self {
            kind,
            job: None,
            own_process_group,
        }
    }

    /// Operating-system process identifier of the direct child.
    pub fn id(&self) -> u32 {
        match &self.kind {
            StdChildKind::Standard(child) => child.id(),
            StdChildKind::ExactTrace(child) => child.id(),
        }
    }

    /// Whether the child is reaped by an exact-trace tracer.
    pub fn is_exact_trace(&self) -> bool {
        matches!(self.kind, StdChildKind::ExactTrace(_))
    }

    /// The standard child handle, for post-spawn containment such as Job
    /// Object assignment. `None` for exact-trace children.
    pub fn std_child(&self) -> Option<&std::process::Child> {
        match &self.kind {
            StdChildKind::Standard(child) => Some(child),
            StdChildKind::ExactTrace(_) => None,
        }
    }

    /// Keep the per-spawn Job Object alive exactly as long as this child.
    pub fn attach_job(&mut self, job: WindowsJobHandle) {
        self.job = Some(job);
    }

    /// Non-blocking exit check. `Ok(None)` while the child is running.
    pub fn try_wait_code(&mut self) -> io::Result<Option<i32>> {
        match &mut self.kind {
            StdChildKind::Standard(child) => child.try_wait().map(|status| status.map(exit_code)),
            StdChildKind::ExactTrace(child) => child.try_wait_code(),
        }
    }

    /// Blocking wait for the exit code.
    ///
    /// An exact-trace child is waited by polling its tracer's published
    /// state. This parks the calling thread, so it must never run on an
    /// actor-runtime worker; the runtime observes exit with
    /// [`Self::try_wait_code`] instead.
    pub fn wait_code(&mut self) -> io::Result<i32> {
        match &mut self.kind {
            StdChildKind::Standard(child) => child.wait().map(exit_code),
            StdChildKind::ExactTrace(child) => loop {
                if let Some(code) = child.try_wait_code()? {
                    return Ok(code);
                }
                std::thread::sleep(TRACED_WAIT_POLL);
            },
        }
    }

    /// Hard-kill the direct child without reaping it.
    pub fn kill(&mut self) -> io::Result<()> {
        match &mut self.kind {
            StdChildKind::Standard(child) => child.kill(),
            StdChildKind::ExactTrace(child) => child.kill(),
        }
    }

    /// Hard-kill the child's own process group when it leads one and the
    /// host can signal groups, otherwise the direct child. Never reaps.
    pub fn kill_group_or_child(&mut self) -> io::Result<()> {
        if self.own_process_group
            && unix_signal_process_group(self.id() as i32, UnixSignalKind::Kill).is_ok()
        {
            return Ok(());
        }
        self.kill()
    }

    pub fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        match &mut self.kind {
            StdChildKind::Standard(child) => child.stdin.take(),
            StdChildKind::ExactTrace(child) => child.take_stdin(),
        }
    }

    pub fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        match &mut self.kind {
            StdChildKind::Standard(child) => child.stdout.take(),
            StdChildKind::ExactTrace(child) => child.take_stdout(),
        }
    }

    pub fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        match &mut self.kind {
            StdChildKind::Standard(child) => child.stderr.take(),
            StdChildKind::ExactTrace(child) => child.take_stderr(),
        }
    }

    /// Take both piped output streams and make them cancellable through
    /// `cancellation` (see `cancel_capture_reader`). The caller keeps sole
    /// ownership of the cancellation state, so dropping its last reference
    /// is the reader-teardown signal it always was. A failure leaves no
    /// half-registered reader.
    pub fn prepare_capture(
        &mut self,
        cancellation: &CaptureCancellation,
    ) -> io::Result<PlatformCaptureReaders> {
        let (Some(stdout), Some(stderr)) = (self.take_stdout(), self.take_stderr()) else {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "child stdout and stderr must both be piped for capture",
            ));
        };
        let stdout = prepare_capture_reader(stdout, cancellation, CaptureStream::Stdout)?;
        let stderr = match prepare_capture_reader(stderr, cancellation, CaptureStream::Stderr) {
            Ok(stderr) => stderr,
            Err(error) => {
                capture_reader_done(cancellation, CaptureStream::Stdout);
                return Err(error);
            }
        };
        Ok(PlatformCaptureReaders { stdout, stderr })
    }

    /// Abandon a child whose launch failed after spawn: kill it and reap it
    /// off the calling thread, so the caller's error path stays bounded.
    pub fn discard_after_start_error(self) {
        let Self { kind, job, .. } = self;
        match kind {
            StdChildKind::Standard(mut child) => {
                let _ = child.kill();
                // Keep ownership until the child is eventually reaped, even if
                // kill delivery takes time; the job outlives the reap.
                std::thread::spawn(move || {
                    let _job = job;
                    let _ = child.wait();
                });
            }
            StdChildKind::ExactTrace(mut child) => {
                // The dedicated tracer remains the sole waiter and will reap it.
                let _ = child.kill();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PlatformStdChild, TRACED_WAIT_POLL};
    use crate::platform::process::CaptureCancellation;
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const FIXTURE_ENV: &str = "RUNNING_PROCESS_STD_CHILD_FIXTURE";

    /// Child half of these tests, selected by the fixture variable.
    #[test]
    fn std_child_fixture() {
        let mode = std::env::var_os(FIXTURE_ENV);
        match mode.as_ref().and_then(|mode| mode.to_str()) {
            Some("exit7") => std::process::exit(7),
            Some("sleep") => std::thread::sleep(Duration::from_secs(60)),
            Some("print") => {
                println!("std-child-stdout");
                eprintln!("std-child-stderr");
            }
            _ => {}
        }
    }

    fn fixture(mode: &str, piped: bool) -> PlatformStdChild {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "std_child::tests::std_child_fixture",
                "--nocapture",
            ])
            .env(FIXTURE_ENV, mode)
            .stdin(Stdio::null());
        if piped {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
        } else {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        PlatformStdChild::from_std(command.spawn().expect("spawn fixture"), false)
    }

    fn poll_exit(child: &mut PlatformStdChild, limit: Duration) -> Option<i32> {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if let Some(code) = child.try_wait_code().expect("try_wait") {
                return Some(code);
            }
            std::thread::sleep(TRACED_WAIT_POLL);
        }
        None
    }

    #[test]
    fn exit_is_observed_by_polling() {
        let mut child = fixture("exit7", false);
        assert!(child.id() > 0);
        assert!(!child.is_exact_trace());
        assert!(child.std_child().is_some());
        assert_eq!(poll_exit(&mut child, Duration::from_secs(30)), Some(7));
        // Once observed, the status stays observable.
        assert_eq!(child.try_wait_code().expect("try_wait"), Some(7));
        assert_eq!(child.wait_code().expect("wait"), 7);
    }

    #[test]
    fn kill_ends_a_running_child() {
        let mut child = fixture("sleep", false);
        let pid = child.id();
        assert_eq!(child.try_wait_code().expect("try_wait"), None);
        child.kill().expect("kill");
        let code = child.wait_code().expect("reap");
        assert_ne!(code, 0);
        assert_eq!(child.id(), pid, "pid is stable across the reap");
    }

    #[test]
    fn group_kill_without_own_group_falls_back_to_the_child() {
        let mut child = fixture("sleep", false);
        child.kill_group_or_child().expect("kill");
        assert!(poll_exit(&mut child, Duration::from_secs(30)).is_some());
    }

    #[test]
    fn capture_readers_drain_and_register_for_cancellation() {
        let mut child = fixture("print", true);
        let cancellation = CaptureCancellation::default();
        let readers = child
            .prepare_capture(&cancellation)
            .expect("prepare capture");
        let collect = |mut reader: Box<dyn Read + Send>| {
            std::thread::spawn(move || {
                let mut text = String::new();
                reader.read_to_string(&mut text).expect("read capture");
                text
            })
        };
        let stdout = collect(readers.stdout);
        let stderr = collect(readers.stderr);
        assert_eq!(child.wait_code().expect("wait"), 0);
        assert!(stdout.join().expect("stdout").contains("std-child-stdout"));
        assert!(stderr.join().expect("stderr").contains("std-child-stderr"));
        // Cancelling after EOF is harmless.
        crate::cancel_capture_reader(&cancellation);
    }

    #[test]
    fn capture_requires_both_pipes() {
        let mut child = fixture("sleep", false);
        let error = child
            .prepare_capture(&CaptureCancellation::default())
            .err()
            .expect("unpiped capture is rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        child.discard_after_start_error();
    }
}
