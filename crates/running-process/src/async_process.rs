//! Native asynchronous pipe-process API.
//!
//! The platform crate owns Tokio's process primitives. This module exposes a
//! stable process-facing API without re-exporting `tokio::process` types.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{ExitStatus, Output};
use std::time::Duration;

use running_process_platform_internal::{SpawnSpec, StreamMode};

use crate::blocking_island::dispatch;
use crate::process_runtime::{block_on, ActorProcess};
use crate::{ProcessError, RunOutput, SharedOutputCursor};

/// Run a blocking OS call on the shared bounded island and flatten the result.
async fn bounded_blocking<T, F>(operation: F) -> std::io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
{
    dispatch(operation).await.map_err(std::io::Error::from)?
}

/// A process configured for asynchronous execution.
pub struct AsyncProcess {
    spec: SpawnSpec,
    child: Option<ActorProcess>,
}

impl AsyncProcess {
    /// Create a direct (non-shell) async process.
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            spec: SpawnSpec::new(program)
                .stdin(StreamMode::Piped)
                .stdout(StreamMode::Piped)
                .stderr(StreamMode::Piped),
            child: None,
        }
    }

    /// Append an argument without requiring UTF-8.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.spec = self.spec.arg(arg);
        self
    }

    /// Set the child working directory.
    pub fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.spec = self.spec.current_dir(path);
        self
    }

    /// Add an environment override.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.spec = self.spec.env(key, value);
        self
    }

    /// Spawn the child into a process group of its own.
    ///
    /// Required for [`Self::terminate_group_soft`] to have anything to
    /// address. It also detaches the child from the parent's console Ctrl+C,
    /// so it is opt-in rather than the default.
    pub fn create_process_group(mut self, create: bool) -> Self {
        self.spec = self.spec.create_process_group(create);
        self
    }

    /// Kill this child if the spawning process dies unexpectedly.
    pub fn kill_when_owner_dies(mut self, kill: bool) -> Self {
        self.spec = self.spec.kill_when_owner_dies(kill);
        self
    }

    /// Start the configured process.
    pub async fn start(&mut self) -> Result<(), ProcessError> {
        if self.child.is_some() {
            return Err(ProcessError::AlreadyStarted);
        }
        self.child = Some(ActorProcess::start(self.spec.clone()).await?);
        Ok(())
    }

    /// Start the process through the canonical actor, for blocking callers.
    ///
    /// This is a compatibility adapter over [`Self::start`], not a second
    /// process engine. It returns [`ProcessError::RuntimeContext`] when called
    /// from a Tokio runtime; use the async method in that context.
    pub fn start_blocking(&mut self) -> Result<(), ProcessError> {
        block_on(self.start())?
    }

    /// Return the child process identifier after [`Self::start`].
    pub async fn pid(&self) -> Result<u32, ProcessError> {
        self.child
            .as_ref()
            .ok_or(ProcessError::NotRunning)?
            .pid()
            .await
    }

    /// Return the child pid through the blocking compatibility adapter.
    pub fn pid_blocking(&self) -> Result<u32, ProcessError> {
        block_on(self.pid())?
    }

    /// Create an independent cursor over output retained by the actor.
    ///
    /// Output is drained when [`Self::output`] or a related capture operation
    /// is requested. A cursor created before capture can therefore observe
    /// records as the capture task appends them, or receive an explicit gap
    /// if the bounded retention window advances past it.
    pub fn output_cursor(&self) -> Result<SharedOutputCursor, ProcessError> {
        Ok(self
            .child
            .as_ref()
            .ok_or(ProcessError::NotRunning)?
            .output_cursor())
    }

    /// Wait for the started process without capturing output.
    pub async fn wait(&self) -> Result<ExitStatus, ProcessError> {
        self.child
            .as_ref()
            .ok_or(ProcessError::NotRunning)?
            .wait()
            .await
    }

    /// Wait through the blocking compatibility adapter.
    pub fn wait_blocking(&mut self) -> Result<ExitStatus, ProcessError> {
        block_on(self.wait())?
    }

    /// Wait for completion, returning [`ProcessError::Timeout`] if the deadline elapses.
    pub async fn wait_timeout(&self, deadline: Duration) -> Result<ExitStatus, ProcessError> {
        tokio::time::timeout(deadline, self.wait())
            .await
            .map_err(|_| ProcessError::Timeout)?
    }

    /// Kill the started process.
    pub async fn kill(&self) -> Result<(), ProcessError> {
        self.child
            .as_ref()
            .ok_or(ProcessError::NotRunning)?
            .kill()
            .await
    }

    /// Kill through the blocking compatibility adapter.
    pub fn kill_blocking(&mut self) -> Result<(), ProcessError> {
        block_on(self.kill())?
    }

    /// Request immediate termination.
    ///
    /// The sync `NativeProcess::terminate` is an alias of `kill`; this keeps
    /// that spelling available on the async surface so a caller porting from
    /// the sync API does not have to rename the call.
    pub async fn terminate(&self) -> Result<(), ProcessError> {
        self.kill().await
    }

    /// Ask the child's process group to shut down gracefully.
    ///
    /// Returns `false` when the process was not configured with
    /// [`Self::create_process_group`], mirroring the sync
    /// `NativeProcess::terminate_group_soft` no-op: there is no group to
    /// address, and the hard-kill schedule is expected to win instead. An
    /// already-exited child is also `false`.
    ///
    /// This is a *request*, not a wait. Follow it with [`Self::wait_timeout`]
    /// and then [`Self::kill`] to bound how long the graceful step is given.
    pub async fn terminate_group_soft(&self) -> Result<bool, ProcessError> {
        self.child
            .as_ref()
            .ok_or(ProcessError::NotRunning)?
            .terminate_group_soft()
            .await
    }

    /// Kill the process and every descendant it has at this moment.
    ///
    /// The tree is a point-in-time snapshot taken by
    /// [`crate::process_tree::kill_tree`], and enumerating it is a blocking OS
    /// operation with no async equivalent on any supported platform. It runs
    /// on the same bounded island the async PTY surface uses, so it can never
    /// occupy more than a fixed number of blocking workers no matter how many
    /// callers request a tree kill at once.
    ///
    /// Returns the number of process instances the OS accepted a kill for.
    pub async fn kill_tree(&self, timeout: Duration) -> Result<u32, ProcessError> {
        let pid = self.pid().await?;
        bounded_blocking(move || crate::process_tree::kill_tree(pid, timeout))
            .await
            .map_err(ProcessError::Io)
    }

    /// Report the exit status if it has already been observed, without waiting.
    ///
    /// This is the async counterpart of `NativeProcess::poll`.
    pub async fn poll(&self) -> Result<Option<ExitStatus>, ProcessError> {
        self.child
            .as_ref()
            .ok_or(ProcessError::NotRunning)?
            .poll()
            .await
    }

    /// Report the exit code if the process has already exited.
    ///
    /// The async counterpart of `NativeProcess::returncode`. Like the sync
    /// method it never blocks; a still-running process reports `None`.
    pub async fn returncode(&self) -> Result<Option<i32>, ProcessError> {
        Ok(self.poll().await?.and_then(|status| status.code()))
    }

    /// Release the actor and its child handles.
    ///
    /// Closes stdin first so a child blocked on input can observe EOF, then
    /// drops the command channel, which ends the actor. Idempotent: closing an
    /// already-closed process succeeds. The child is *not* killed -- this
    /// mirrors `NativeProcess::close`, which releases handles rather than
    /// terminating. Call [`Self::kill`] first if you need the child gone.
    pub async fn close(&mut self) -> Result<(), ProcessError> {
        let Some(child) = self.child.take() else {
            return Ok(());
        };
        // A closed stdin is best-effort: the child may already have exited,
        // which is not a failure of close.
        let _ = child.close_stdin().await;
        drop(child);
        Ok(())
    }

    /// Write bytes to the child's piped stdin without closing it.
    ///
    /// The actor owns the pipe for the complete operation. Cancelling this
    /// future before actor acknowledgement leaves no guarantee whether a
    /// dispatched write reached the child; callers that need an EOF must call
    /// [`Self::close_stdin`] explicitly after a successful write.
    pub async fn write_stdin(&self, bytes: impl AsRef<[u8]>) -> Result<(), ProcessError> {
        self.child
            .as_ref()
            .ok_or(ProcessError::NotRunning)?
            .write_stdin(bytes.as_ref().to_vec())
            .await
    }

    /// Write stdin through the blocking compatibility adapter.
    pub fn write_stdin_blocking(&mut self, bytes: impl AsRef<[u8]>) -> Result<(), ProcessError> {
        let bytes = bytes.as_ref().to_vec();
        block_on(self.write_stdin(bytes))?
    }

    /// Close the child's piped stdin and deliver EOF.
    ///
    /// The operation is idempotent after a successful start.
    pub async fn close_stdin(&self) -> Result<(), ProcessError> {
        self.child
            .as_ref()
            .ok_or(ProcessError::NotRunning)?
            .close_stdin()
            .await
    }

    /// Close stdin through the blocking compatibility adapter.
    pub fn close_stdin_blocking(&mut self) -> Result<(), ProcessError> {
        block_on(self.close_stdin())?
    }

    /// Wait for completion and return captured stdout/stderr.
    pub async fn output(&self) -> Result<RunOutput, ProcessError> {
        let child = self.child.as_ref().ok_or(ProcessError::NotRunning)?;
        let output = child.output().await?;
        Ok(run_output(output))
    }

    /// Capture output through the blocking compatibility adapter.
    pub fn output_blocking(&mut self) -> Result<RunOutput, ProcessError> {
        block_on(self.output())?
    }

    /// Wait for completion and capture output, returning [`ProcessError::Timeout`] if the deadline elapses.
    pub async fn output_timeout(&self, deadline: Duration) -> Result<RunOutput, ProcessError> {
        tokio::time::timeout(deadline, self.output())
            .await
            .map_err(|_| ProcessError::Timeout)?
    }

    /// Wait for completion and capture stdout/stderr within an aggregate byte limit.
    pub async fn output_bounded(&self, limit: usize) -> Result<RunOutput, ProcessError> {
        let child = self.child.as_ref().ok_or(ProcessError::NotRunning)?;
        let output = child.output_bounded(limit).await?;
        Ok(run_output(output))
    }

    /// Capture bounded output through the blocking compatibility adapter.
    pub fn output_bounded_blocking(&mut self, limit: usize) -> Result<RunOutput, ProcessError> {
        block_on(self.output_bounded(limit))?
    }

    /// Spawn, wait, and capture a process in one asynchronous operation.
    pub async fn run(
        program: impl Into<OsString>,
        args: &[OsString],
    ) -> Result<RunOutput, ProcessError> {
        let mut process = Self::new(program);
        for arg in args {
            process = process.arg(arg.clone());
        }
        process.output_after_start().await
    }

    /// Spawn, wait, and capture a process with an aggregate stdout/stderr limit.
    pub async fn run_bounded(
        program: impl Into<OsString>,
        args: &[OsString],
        limit: usize,
    ) -> Result<RunOutput, ProcessError> {
        let mut process = Self::new(program);
        for arg in args {
            process = process.arg(arg.clone());
        }
        process.start().await?;
        process.output_bounded(limit).await
    }

    /// Spawn, wait, and capture a process with an execution deadline.
    pub async fn run_timeout(
        program: impl Into<OsString>,
        args: &[OsString],
        deadline: Duration,
    ) -> Result<RunOutput, ProcessError> {
        let mut process = Self::new(program);
        for arg in args {
            process = process.arg(arg.clone());
        }
        process.start().await?;
        process.output_timeout(deadline).await
    }

    /// Spawn, wait, and capture through the blocking compatibility adapter.
    pub fn run_blocking(
        program: impl Into<OsString>,
        args: &[OsString],
    ) -> Result<RunOutput, ProcessError> {
        let program = program.into();
        let args = args.to_vec();
        block_on(Self::run(program, &args))?
    }

    async fn output_after_start(mut self) -> Result<RunOutput, ProcessError> {
        self.start().await?;
        self.output().await
    }
}

fn run_output(output: Output) -> RunOutput {
    RunOutput {
        stdout: output.stdout,
        stderr: output.stderr,
        exit_code: running_process_platform_internal::platform::process::exit_code(output.status),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::time::Duration;

    use super::AsyncProcess;

    fn fixture_program() -> OsString {
        let exe = std::env::current_exe().expect("test executable path");
        let dir = exe
            .parent()
            .and_then(std::path::Path::parent)
            .expect("test binary should live in <profile>/deps/");
        dir.join(format!(
            "testbin-stdio-scripted{}",
            std::env::consts::EXE_SUFFIX
        ))
        .into_os_string()
    }

    fn fixture(directives: &[&str]) -> AsyncProcess {
        directives.iter().fold(
            AsyncProcess::new(fixture_program()),
            |process, directive| process.arg(*directive),
        )
    }

    #[test]
    fn async_process_owner_death_is_opt_in() {
        let _process = AsyncProcess::new("unused").kill_when_owner_dies(true);
    }

    #[tokio::test]
    async fn async_process_captures_stdout_and_stderr() {
        let process = fixture(&["out:out", "err:err"]);
        let output = process.output_after_start().await.expect("async output");
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
        assert_eq!(output.exit_code, 0);
    }

    #[tokio::test]
    async fn async_process_rejects_double_start() {
        let mut process = fixture(&["exit:0"]);
        process.start().await.expect("first start");
        assert!(matches!(
            process.start().await,
            Err(crate::ProcessError::AlreadyStarted)
        ));
        process.kill().await.ok();
    }

    #[tokio::test]
    async fn async_process_bounded_output_drains_and_reports_overflow() {
        let mut process = fixture(&["out:123456789"]);
        process.start().await.expect("async process starts");
        assert!(matches!(
            process.output_bounded(4).await,
            Err(crate::ProcessError::OutputLimitExceeded { limit: 4 })
        ));
    }

    #[tokio::test]
    async fn async_process_run_bounded_captures_within_limit() {
        let args = vec![OsString::from("out:ok")];
        let output = AsyncProcess::run_bounded(fixture_program(), &args, 16)
            .await
            .expect("bounded run");
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout, b"ok");
    }

    #[tokio::test]
    async fn async_process_output_cursor_observes_actor_capture() {
        let mut process = fixture(&["out:cursor-out", "err:cursor-err"]);
        process.start().await.expect("async process starts");
        let mut cursor = process.output_cursor().expect("output cursor");
        process.output().await.expect("capture output");
        let mut records = Vec::new();
        while let crate::CursorRead::Record(record) = cursor.read_next() {
            records.push(record);
        }
        assert!(records
            .iter()
            .any(|record| record.bytes.windows(6).any(|w| w == b"cursor")));
        assert!(records
            .iter()
            .any(|record| record.stream == crate::StreamKind::Stdout));
        assert!(records
            .iter()
            .any(|record| record.stream == crate::StreamKind::Stderr));
    }

    #[tokio::test]
    async fn async_output_cursor_reaches_terminal_eof_without_polling() {
        let mut process = fixture(&["out:cursor"]);
        process.start().await.expect("async process starts");
        let mut cursor = process.output_cursor().expect("output cursor");
        process.output().await.expect("capture output");
        while !matches!(cursor.read_next_async().await, crate::CursorRead::Eof) {}
        assert!(cursor.is_closed());
    }

    #[test]
    fn blocking_adapter_uses_the_actor_engine() {
        let args = vec![OsString::from("out:blocking")];
        let output =
            AsyncProcess::run_blocking(fixture_program(), &args).expect("blocking actor adapter");
        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.starts_with(b"blocking"));
    }

    #[tokio::test]
    async fn blocking_adapter_rejects_tokio_context_without_deadlocking() {
        let mut process = fixture(&["exit:0"]);
        assert!(matches!(
            process.start_blocking(),
            Err(crate::ProcessError::RuntimeContext)
        ));
    }

    #[tokio::test]
    async fn async_process_timeout_is_explicit_and_kill_remains_available() {
        let mut process = fixture(&["sleep-ms:30000"]);
        process.start().await.expect("async process starts");
        assert!(matches!(
            process.wait_timeout(Duration::from_millis(20)).await,
            Err(crate::ProcessError::Timeout)
        ));
        process.kill().await.expect("kill after timeout");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_process_writes_then_closes_stdin_through_the_actor() {
        let mut process = fixture(&["echo"]);
        process.start().await.expect("async process starts");
        process
            .write_stdin(b"actor-input")
            .await
            .expect("actor writes stdin");
        process.close_stdin().await.expect("actor closes stdin");
        process
            .close_stdin()
            .await
            .expect("stdin close is idempotent");

        let output = process.output().await.expect("actor captures output");
        assert_eq!(output.stdout, b"actor-input");
        assert_eq!(output.exit_code, 0);
    }
}
