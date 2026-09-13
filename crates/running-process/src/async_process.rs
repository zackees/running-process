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
use crate::process_runtime::{block_on, ActorProcess, SessionProcess};
use crate::{ProcessError, RunOutput, SharedOutputCursor};

/// Semantic stdio policy for one asynchronous child stream.
///
/// This intentionally names no runtime, descriptor, handle, or platform type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncStdio {
    /// Leave the stream connected to the parent process.
    Inherit,
    /// Connect the stream to the canonical actor's capture pipe.
    Piped,
    /// Connect the stream to the host null device.
    Null,
}

impl AsyncStdio {
    fn into_internal(self) -> StreamMode {
        match self {
            Self::Inherit => StreamMode::Inherit,
            Self::Piped => StreamMode::Piped,
            Self::Null => StreamMode::Null,
        }
    }
}

/// Status-preserving one-shot asynchronous capture result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsyncCapturedOutput {
    /// The operating system's unmodified child status.
    pub status: ExitStatus,
    /// Bytes captured from stdout.
    pub stdout: Vec<u8>,
    /// Bytes captured from stderr.
    pub stderr: Vec<u8>,
}

/// Bounds and terminal-owner policy for an [`AsyncProcessSession`].
///
/// The session has one bounded, lossless output queue. When it fills, output
/// pumps apply backpressure to their matching child pipe rather than evicting
/// chunks. Each output chunk is no larger than `max_chunk_bytes`; the queue
/// holds at most `max_queued_chunks` chunks. At most one extra chunk per
/// stream can be waiting to enter that queue, so the maximum retained payload
/// is `(max_queued_chunks + 4) * max_chunk_bytes`: two reader buffers and up
/// to two chunks awaiting queue capacity are included. The direct-child exit
/// wait is independent of that queue and of pipe EOF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AsyncProcessSessionOptions {
    /// Maximum number of output events retained before pumps backpressure.
    pub max_queued_chunks: usize,
    /// Maximum bytes in any one output chunk.
    pub max_chunk_bytes: usize,
    /// Post-exit pipe-read policy after the direct child is reaped.
    ///
    /// `None` waits for normal EOF without a watchdog. `Some(duration)` is a
    /// cumulative pipe-read budget after direct-child exit, after which a
    /// still-open stream is abandoned. Time spent delivering a bounded queued
    /// chunk does not consume that budget. `Some(Duration::ZERO)` abandons a
    /// genuinely pending read immediately, but still delivers pipe bytes that
    /// are already readable at that boundary.
    pub post_exit_grace: Option<Duration>,
    /// Whether dropping the terminal session owner terminates and reaps the direct child.
    pub kill_on_drop: bool,
    /// On owner drop, attempt a bounded descendant snapshot sweep before
    /// direct-child fallback. Implies direct-child cleanup even when
    /// `kill_on_drop` is false. Defaults to false for compatibility.
    ///
    /// Currently applies only while the actor retains an unreaped root;
    /// it does not recover descendant ownership after the root was reaped.
    pub kill_tree_on_drop: bool,
}

impl Default for AsyncProcessSessionOptions {
    fn default() -> Self {
        Self {
            max_queued_chunks: 256,
            max_chunk_bytes: 8 * 1024,
            post_exit_grace: Some(Duration::from_millis(250)),
            kill_on_drop: true,
            kill_tree_on_drop: false,
        }
    }
}

/// One bounded, stream-tagged output chunk from an [`AsyncProcessSession`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsyncProcessSessionChunk {
    /// Stream that produced these bytes.
    pub stream: crate::StreamKind,
    /// Raw output bytes, bounded by the configured chunk limit.
    pub bytes: Vec<u8>,
}

/// Output lifecycle event from an [`AsyncProcessSession`].
///
/// The two stream end variants are deliberately independent from
/// [`AsyncProcessSession::wait`]: a direct child may exit while a descendant
/// still holds one inherited pipe open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsyncProcessSessionEvent {
    /// A stream-tagged output chunk.
    Chunk(AsyncProcessSessionChunk),
    /// One output pipe reached EOF normally.
    StreamEof(crate::StreamKind),
    /// The post-exit pipe-read grace elapsed while this pipe remained open, so
    /// the session explicitly abandoned and closed its reader.
    StreamAbandoned(crate::StreamKind),
    /// A stream reader failed before normal EOF. No unreported replacement
    /// data is synthesized; callers retain the direct-child lifecycle lane.
    StreamError {
        /// Reader that failed.
        stream: crate::StreamKind,
        /// Portable category of the underlying I/O failure.
        kind: std::io::ErrorKind,
        /// Human-readable message from the underlying I/O failure.
        ///
        /// This is kept alongside the portable kind so facade clients can
        /// retain their durable diagnostics without exposing an I/O handle.
        message: String,
        /// Native operating-system error, when the reader exposed one.
        raw_os_error: Option<i32>,
    },
}

impl From<Output> for AsyncCapturedOutput {
    fn from(output: Output) -> Self {
        Self {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        }
    }
}

/// Semantic description of an asynchronous child process.
///
/// The concrete spawn machinery remains private. The builder feeds the same
/// canonical actor as [`AsyncProcess::new`], not a second execution engine.
pub struct AsyncProcessBuilder {
    spec: SpawnSpec,
    kill_on_drop: bool,
}

impl AsyncProcessBuilder {
    /// Describe a direct program invocation.
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            kill_on_drop: false,
            spec: SpawnSpec::new(program)
                .stdin(StreamMode::Piped)
                .stdout(StreamMode::Piped)
                .stderr(StreamMode::Piped),
        }
    }

    /// Describe a command using the platform-owned shell convention.
    pub fn shell(command: impl Into<OsString>) -> Self {
        Self {
            kill_on_drop: false,
            spec: running_process_platform_internal::shell_spec(command.into())
                .stdin(StreamMode::Piped)
                .stdout(StreamMode::Piped)
                .stderr(StreamMode::Piped),
        }
    }

    /// Append one argument without requiring UTF-8.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.spec = self.spec.arg(arg);
        self
    }

    /// Append several arguments without requiring UTF-8.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        for arg in args {
            self.spec = self.spec.arg(arg);
        }
        self
    }

    /// Set the child working directory.
    pub fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.spec = self.spec.current_dir(path);
        self
    }

    /// Add one environment override.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.spec = self.spec.env(key, value);
        self
    }

    /// Remove an inherited entry or an earlier override. Later overrides win.
    pub fn env_remove(mut self, key: impl Into<OsString>) -> Self {
        self.spec = self.spec.env_remove(key);
        self
    }

    /// Suppress a Windows console window without changing stdio or containment.
    /// Defaults to false; has no effect on Unix.
    pub fn hide_console(mut self, hide: bool) -> Self {
        self.spec = self.spec.hide_console(hide);
        self
    }

    /// Clear inherited environment entries before applying [`Self::env`].
    pub fn clear_env(mut self, clear: bool) -> Self {
        self.spec = self.spec.clear_env(clear);
        self
    }

    /// Configure child stdin.
    pub fn stdin(mut self, mode: AsyncStdio) -> Self {
        self.spec = self.spec.stdin(mode.into_internal());
        self
    }

    /// Configure child stdout.
    pub fn stdout(mut self, mode: AsyncStdio) -> Self {
        self.spec = self.spec.stdout(mode.into_internal());
        self
    }

    /// Configure child stderr.
    pub fn stderr(mut self, mode: AsyncStdio) -> Self {
        self.spec = self.spec.stderr(mode.into_internal());
        self
    }

    /// Spawn the child in its own process group.
    pub fn create_process_group(mut self, create: bool) -> Self {
        self.spec = self.spec.create_process_group(create);
        self
    }

    /// Kill the child if its spawning owner dies.
    pub fn kill_when_owner_dies(mut self, kill: bool) -> Self {
        self.spec = self.spec.kill_when_owner_dies(kill);
        self
    }

    /// Apply the host's existing niceness policy at child creation.
    ///
    /// Unix receives the requested nice value. Windows applies its existing
    /// coarse priority-class mapping rather than treating the number as a
    /// portable Unix-nice equivalent.
    pub fn nice(mut self, nice: Option<i32>) -> Self {
        self.spec = self.spec.nice(nice);
        self
    }

    /// Apply canonical scheduling intent. Last call wins with [`Self::nice`].
    pub fn priority(mut self, priority: crate::ProcessPriority) -> Self {
        self.spec = self.spec.priority(priority);
        self
    }

    /// Attempt scheduling adjustment after spawn; denial leaves the child
    /// running at its existing priority. The child may run before adjustment.
    pub fn priority_best_effort(mut self, priority: crate::ProcessPriority) -> Self {
        self.spec = self.spec.priority_best_effort(priority);
        self
    }

    /// Acquire exclusion on the native spawning thread, not on the caller's
    /// async task. The permit covers native process creation and exec result.
    pub fn spawn_admission(mut self, admission: crate::SpawnAdmission) -> Self {
        self.spec = self.spec.spawn_admission(admission);
        self
    }

    /// Build an [`AsyncProcess`] backed by the canonical actor.
    pub fn build(self) -> AsyncProcess {
        let mut process = AsyncProcess::from_spec(self.spec);
        process.kill_on_drop = self.kill_on_drop;
        process
    }

    /// Kill and reap a running child when its async handle is dropped.
    ///
    /// Defaults to false. Cleanup is performed by the library actor, including
    /// when an output capture owns the child. This is independent of OS-level
    /// spawning-owner death policy. Sessions use their explicit options instead.
    pub fn kill_on_drop(mut self, kill: bool) -> Self {
        self.kill_on_drop = kill;
        self
    }

    /// Build a long-lived, concurrently pumped process session.
    pub fn session(self, options: AsyncProcessSessionOptions) -> AsyncProcessSession {
        AsyncProcessSession::from_spec(self.spec, options)
    }

    /// Build a session coupled to an observation subscriber. The process is
    /// still spawned only when [`AsyncProcessSession::start`] is called; the
    /// observer is installed at that native spawn boundary, rather than
    /// attached after the child or its descendants may already exist.
    pub fn session_with_observer(
        self,
        options: AsyncProcessSessionOptions,
        observer: crate::ObserverConfig,
    ) -> (AsyncProcessSession, crate::ObserverSubscriber) {
        let (emitter, subscriber) = crate::observer::ObserverEmitter::new(observer);
        (
            AsyncProcessSession::from_spec_with_observer(self.spec, options, Some(emitter)),
            subscriber,
        )
    }

    /// Start and capture both output streams while preserving [`ExitStatus`].
    pub async fn capture(self) -> Result<AsyncCapturedOutput, ProcessError> {
        let mut process = self.build();
        process.start().await?;
        process.capture().await
    }

    /// Start and capture both streams within one aggregate byte ceiling.
    pub async fn capture_bounded(self, limit: usize) -> Result<AsyncCapturedOutput, ProcessError> {
        let mut process = self.build();
        process.start().await?;
        process.capture_bounded(limit).await
    }
}

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
    kill_on_drop: bool,
}

impl AsyncProcess {
    fn from_spec(spec: SpawnSpec) -> Self {
        Self {
            spec,
            child: None,
            kill_on_drop: false,
        }
    }

    /// Create a direct (non-shell) async process.
    pub fn new(program: impl Into<OsString>) -> Self {
        AsyncProcessBuilder::new(program).build()
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

    /// Apply the host's existing niceness policy at child creation.
    pub fn nice(mut self, nice: Option<i32>) -> Self {
        self.spec = self.spec.nice(nice);
        self
    }

    /// Start the configured process.
    pub async fn start(&mut self) -> Result<(), ProcessError> {
        if self.child.is_some() {
            return Err(ProcessError::AlreadyStarted);
        }
        self.child =
            Some(ActorProcess::start_with_drop_policy(self.spec.clone(), self.kill_on_drop).await?);
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

    /// Capture both streams while preserving the operating system exit status.
    pub async fn capture(&self) -> Result<AsyncCapturedOutput, ProcessError> {
        let child = self.child.as_ref().ok_or(ProcessError::NotRunning)?;
        Ok(child.output().await?.into())
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
    ///
    /// This is a retention limit, not an execution deadline or kill-on-overflow
    /// policy. After overflow the readers discard further bytes until EOF;
    /// once the child exits and pipes close, this returns
    /// [`ProcessError::OutputLimitExceeded`]. A producer that never exits still
    /// needs caller-owned timeout/cancellation and lifecycle cleanup.
    pub async fn output_bounded(&self, limit: usize) -> Result<RunOutput, ProcessError> {
        let child = self.child.as_ref().ok_or(ProcessError::NotRunning)?;
        let output = child.output_bounded(limit).await?;
        Ok(run_output(output))
    }

    /// Capture both streams within one aggregate byte limit and preserve status.
    /// Uses the same wait-for-completion retention policy as
    /// [`Self::output_bounded`]; overflow does not terminate the child.
    pub async fn capture_bounded(&self, limit: usize) -> Result<AsyncCapturedOutput, ProcessError> {
        let child = self.child.as_ref().ok_or(ProcessError::NotRunning)?;
        Ok(child.output_bounded(limit).await?.into())
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

/// A long-lived process session with independent lifecycle and output lanes.
///
/// The session is intentionally a terminal owner: it is not cloneable. Its
/// drop policy is explicit in [`AsyncProcessSessionOptions::kill_on_drop`],
/// and the actor always reaps the direct child before completing cleanup.
pub struct AsyncProcessSession {
    spec: SpawnSpec,
    options: AsyncProcessSessionOptions,
    observer: Option<crate::observer::ObserverEmitter>,
    control: Option<AsyncProcessSessionControl>,
    output: Option<AsyncProcessSessionOutput>,
}

/// Terminal lifecycle and control lane split from an [`AsyncProcessSession`].
///
/// This is the sole terminal owner after [`AsyncProcessSession::into_parts`].
/// Dropping it activates the configured [`AsyncProcessSessionOptions::kill_on_drop`]
/// cleanup policy even if an [`AsyncProcessSessionOutput`] is still receiving
/// already-pumped events. It is intentionally not cloneable.
pub struct AsyncProcessSessionControl {
    process: SessionProcess,
}

/// Coverage confirmed by an explicit session tree-termination request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessTreeKill {
    /// Every member of the captured descendant snapshot reached terminal state.
    /// This is not containment of descendants created after the snapshot.
    TreeKilled,
    /// The tree sweep was unavailable or failed; only the owned direct child
    /// was confirmed terminated. Descendant cleanup is not confirmed.
    ProcessKilled,
}

/// Single-consumer output lane split from an [`AsyncProcessSession`].
///
/// Dropping this receiver detaches output delivery only: pumps continue to
/// drain/discard their pipes and the paired [`AsyncProcessSessionControl`]
/// remains the terminal owner. It is intentionally not cloneable.
pub struct AsyncProcessSessionOutput {
    output: tokio::sync::mpsc::Receiver<AsyncProcessSessionEvent>,
}

impl AsyncProcessSession {
    fn from_spec(spec: SpawnSpec, options: AsyncProcessSessionOptions) -> Self {
        Self::from_spec_with_observer(spec, options, None)
    }

    fn from_spec_with_observer(
        spec: SpawnSpec,
        options: AsyncProcessSessionOptions,
        observer: Option<crate::observer::ObserverEmitter>,
    ) -> Self {
        Self {
            spec,
            options,
            observer,
            control: None,
            output: None,
        }
    }

    /// Start direct-child lifecycle observation and both output pumps.
    pub async fn start(&mut self) -> Result<(), ProcessError> {
        if self.control.is_some() {
            return Err(ProcessError::AlreadyStarted);
        }
        let (process, output) =
            SessionProcess::start(self.spec.clone(), self.options, self.observer.take()).await?;
        self.control = Some(AsyncProcessSessionControl { process });
        self.output = Some(AsyncProcessSessionOutput { output });
        Ok(())
    }

    /// Split a started session into independently awaitable control and output
    /// lanes.
    ///
    /// The control lane remains the sole terminal owner, so dropping either
    /// returned handle cannot orphan the direct child: dropping control
    /// activates its configured cleanup policy, while dropping output leaves
    /// pumps draining/discarding until lifecycle cleanup completes.
    pub fn into_parts(
        mut self,
    ) -> Result<(AsyncProcessSessionControl, AsyncProcessSessionOutput), ProcessError> {
        match (self.control.take(), self.output.take()) {
            (Some(control), Some(output)) => Ok((control, output)),
            _ => Err(ProcessError::NotRunning),
        }
    }

    /// Return the direct child's launch-time numeric identifier.
    ///
    /// It is diagnostic only. Session controls remain bound to the actor's
    /// owned child identity and never target this cached number.
    pub fn pid(&self) -> Result<u32, ProcessError> {
        self.control
            .as_ref()
            .map(AsyncProcessSessionControl::pid)
            .ok_or(ProcessError::NotRunning)
    }

    /// Receive the next lossless output event.
    ///
    /// `None` follows normal EOF, an explicit post-exit abandonment, or an
    /// explicitly reported reader error. A slow receiver causes bounded
    /// producer backpressure; it never silently evicts compiler output.
    pub async fn next_output(&mut self) -> Option<AsyncProcessSessionEvent> {
        self.output.as_mut()?.next_output().await
    }

    /// Wait only for the direct child to exit and be reaped.
    pub async fn wait(&self) -> Result<ExitStatus, ProcessError> {
        self.control
            .as_ref()
            .ok_or(ProcessError::NotRunning)?
            .wait()
            .await
    }

    /// Observe whether the direct child exit has already been reaped.
    pub async fn poll(&self) -> Result<Option<ExitStatus>, ProcessError> {
        self.control
            .as_ref()
            .ok_or(ProcessError::NotRunning)?
            .poll()
            .await
    }

    /// Directly terminate and reap the child. Output delivery remains open
    /// until its normal EOF or configured post-exit grace.
    pub async fn kill(&self) -> Result<(), ProcessError> {
        self.control
            .as_ref()
            .ok_or(ProcessError::NotRunning)?
            .kill()
            .await
    }

    /// Terminate a descendant snapshot through the exclusive native owner.
    ///
    /// Returns the weaker direct-child outcome when full snapshot cleanup
    /// cannot be confirmed. An already-reaped session is an error, not a fresh
    /// lookup of its former PID. Dropping this request does not cancel a native
    /// sweep already accepted by the actor.
    pub async fn kill_tree(&self, timeout: Duration) -> Result<ProcessTreeKill, ProcessError> {
        self.control
            .as_ref()
            .ok_or(ProcessError::NotRunning)?
            .kill_tree(timeout)
            .await
    }

    /// Request graceful termination for an explicitly child-owned group.
    ///
    /// Returns `false` when no child-owned group was configured. Hosts that
    /// cannot safely prove the launch identity report an I/O error rather
    /// than targeting a numeric group that might have been reused.
    pub async fn terminate_group_soft(&self) -> Result<bool, ProcessError> {
        self.control
            .as_ref()
            .ok_or(ProcessError::NotRunning)?
            .terminate_group_soft()
            .await
    }

    /// Write and flush one bounded chunk to piped stdin.
    ///
    /// A write longer than [`AsyncProcessSessionOptions::max_chunk_bytes`]
    /// returns `InvalidInput`. Writes use a separately bounded input worker,
    /// holding at most `max_queued_chunks` pending chunks plus one active
    /// write, so a slow child read never blocks lifecycle controls.
    pub async fn write_stdin(&self, bytes: impl AsRef<[u8]>) -> Result<(), ProcessError> {
        self.control
            .as_ref()
            .ok_or(ProcessError::NotRunning)?
            .write_stdin(bytes.as_ref().to_vec())
            .await
    }

    /// Close piped stdin, delivering EOF to the direct child.
    pub async fn close_stdin(&self) -> Result<(), ProcessError> {
        self.control
            .as_ref()
            .ok_or(ProcessError::NotRunning)?
            .close_stdin()
            .await
    }

    /// Sample direct-child CPU time when the selected host can prove the
    /// launch identity is still the same process. Unsupported hosts and
    /// unavailable identities return `None`.
    pub async fn cpu_time(&self) -> Result<Option<Duration>, ProcessError> {
        self.control
            .as_ref()
            .ok_or(ProcessError::NotRunning)?
            .cpu_time()
            .await
    }
}

impl AsyncProcessSessionControl {
    /// Return the direct child's launch-time numeric identifier.
    ///
    /// It is diagnostic only. Controls remain bound to the actor's owned
    /// child identity and never target this cached number.
    pub fn pid(&self) -> u32 {
        self.process.pid()
    }

    /// Wait only for the direct child to exit and be reaped.
    pub async fn wait(&self) -> Result<ExitStatus, ProcessError> {
        self.process.wait().await
    }

    /// Observe whether the direct child exit has already been reaped.
    pub async fn poll(&self) -> Result<Option<ExitStatus>, ProcessError> {
        self.process.poll().await
    }

    /// Directly terminate and reap the child.
    ///
    /// Output delivery remains open until normal EOF or the configured
    /// post-exit grace, independently of this control request.
    pub async fn kill(&self) -> Result<(), ProcessError> {
        self.process.kill().await
    }

    /// Terminate the captured tree through the actor and confirm direct-child
    /// reaping within the supplied total request deadline.
    pub async fn kill_tree(&self, timeout: Duration) -> Result<ProcessTreeKill, ProcessError> {
        self.process.kill_tree(timeout).await
    }

    /// Request graceful termination for an explicitly child-owned group.
    pub async fn terminate_group_soft(&self) -> Result<bool, ProcessError> {
        self.process.terminate_group_soft().await
    }

    /// Write and flush one bounded chunk to piped stdin.
    pub async fn write_stdin(&self, bytes: impl AsRef<[u8]>) -> Result<(), ProcessError> {
        self.process.write_stdin(bytes.as_ref().to_vec()).await
    }

    /// Close piped stdin, delivering EOF to the direct child.
    pub async fn close_stdin(&self) -> Result<(), ProcessError> {
        self.process.close_stdin().await
    }

    /// Sample direct-child CPU time when the host supports identity-safe
    /// accounting. Unsupported hosts and unavailable identities return `None`.
    pub async fn cpu_time(&self) -> Result<Option<Duration>, ProcessError> {
        self.process.cpu_time().await
    }
}

impl AsyncProcessSessionOutput {
    /// Receive the next lossless output event from this session's only output
    /// consumer lane.
    ///
    /// `None` follows normal EOF, explicit post-exit abandonment, or a
    /// reported reader error. A slow receiver causes bounded producer
    /// backpressure; it never silently evicts compiler output.
    pub async fn next_output(&mut self) -> Option<AsyncProcessSessionEvent> {
        self.output.recv().await
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

    use super::{AsyncProcess, AsyncProcessBuilder, AsyncProcessSessionOptions, AsyncStdio};

    #[test]
    fn drop_policy_is_explicit_and_preserved_by_builder() {
        assert!(!AsyncProcessBuilder::new("unused").build().kill_on_drop);
        assert!(
            AsyncProcessBuilder::new("unused")
                .kill_on_drop(true)
                .build()
                .kill_on_drop
        );
        assert!(
            !AsyncProcessBuilder::new("unused")
                .kill_on_drop(true)
                .kill_on_drop(false)
                .build()
                .kill_on_drop
        );
    }

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
    async fn admission_keeps_non_send_permit_on_native_spawn_thread() {
        use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};
        static EXCLUSION: RwLock<()> = RwLock::new(());
        struct Permit {
            _guard: RwLockReadGuard<'static, ()>,
            acquired_on: std::thread::ThreadId,
            released: Arc<Mutex<Option<std::thread::ThreadId>>>,
        }
        impl Drop for Permit {
            fn drop(&mut self) {
                assert_eq!(self.acquired_on, std::thread::current().id());
                *self.released.lock().unwrap() = Some(self.acquired_on);
            }
        }
        let released = Arc::new(Mutex::new(None));
        let marker = released.clone();
        let admission = crate::SpawnAdmission::new(move || {
            Ok(Permit {
                _guard: EXCLUSION.read().unwrap(),
                acquired_on: std::thread::current().id(),
                released: marker.clone(),
            })
        });
        let mut process = AsyncProcessBuilder::new(fixture_program())
            .arg("exit:0")
            .kill_on_drop(true)
            .spawn_admission(admission)
            .build();
        let start = process.start();
        fn assert_send<T: Send>(_: &T) {}
        assert_send(&start);
        tokio::time::timeout(Duration::from_secs(5), start)
            .await
            .unwrap()
            .unwrap();
        assert!(
            released.lock().unwrap().is_some(),
            "spawn acknowledgement must follow permit release"
        );
        assert!(
            EXCLUSION.try_write().is_ok(),
            "admission must not span child lifetime"
        );
        assert!(tokio::time::timeout(Duration::from_secs(5), process.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
    }

    #[tokio::test]
    async fn admission_denial_precedes_native_exec_and_retains_io_kind() {
        let admission = crate::SpawnAdmission::new(|| {
            Err::<(), _>(std::io::ErrorKind::PermissionDenied.into())
        });
        let mut process = AsyncProcessBuilder::new("rp-nonexistent-denied-spawn-fixture")
            .kill_on_drop(true)
            .spawn_admission(admission)
            .build();
        let result = tokio::time::timeout(Duration::from_secs(5), process.start())
            .await
            .unwrap();
        assert!(matches!(result, Err(crate::ProcessError::Spawn(error))
            if error.kind() == std::io::ErrorKind::PermissionDenied));
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
    async fn observed_session_emits_lifecycle_from_its_single_actor_spawn() {
        let (mut session, subscriber) = AsyncProcessBuilder::new(fixture_program())
            .arg("exit:0")
            .session_with_observer(
                AsyncProcessSessionOptions::default(),
                crate::ObserverConfig::lifecycle(),
            );
        session.start().await.expect("session starts");
        let pid = session.pid().expect("started pid");
        let started = subscriber
            .recv_timeout(Duration::from_secs(5))
            .expect("started event");
        assert_eq!(started.category, crate::EventCategory::Lifecycle);
        assert_eq!(started.kind, crate::ObserverEventKind::Started);
        assert_eq!(started.pid, pid);
        assert!(session.wait().await.expect("session exits").success());
        let exited = subscriber
            .recv_timeout(Duration::from_secs(5))
            .expect("exited event");
        assert_eq!(exited.category, crate::EventCategory::Lifecycle);
        assert_eq!(
            exited.kind,
            crate::ObserverEventKind::Exited { exit_code: 0 }
        );
        assert_eq!(exited.pid, pid);
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

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bounded_capture_retention_overflow_does_not_terminate_the_producer() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            let mut process = AsyncProcessBuilder::new("/bin/sh")
                .args([
                    "-c",
                    "head -c 1048576 /dev/zero; printf complete > completed",
                ])
                .current_dir(directory.path())
                .stdin(AsyncStdio::Null)
                .stdout(AsyncStdio::Piped)
                .stderr(AsyncStdio::Piped)
                .kill_on_drop(true)
                .build();
            process.start().await.expect("start overflowing producer");
            process.capture_bounded(4).await
        })
        .await
        .expect("finite producer must finish despite overflow");
        assert!(matches!(
            result,
            Err(crate::ProcessError::OutputLimitExceeded { limit: 4 })
        ));
        assert_eq!(
            std::fs::read(directory.path().join("completed")).unwrap(),
            b"complete"
        );
    }

    #[tokio::test]
    async fn semantic_capture_preserves_status_and_both_streams() {
        let output = AsyncProcessBuilder::new(fixture_program())
            .arg("out:out")
            .arg("err:err")
            .arg("exit:7")
            .stdin(AsyncStdio::Null)
            .stdout(AsyncStdio::Piped)
            .stderr(AsyncStdio::Piped)
            .capture()
            .await
            .expect("nonzero exit is a capture result");
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
    }

    #[tokio::test]
    async fn semantic_bounded_capture_uses_the_existing_aggregate_limit() {
        let result = AsyncProcessBuilder::new(fixture_program())
            .arg("out:123456789")
            .capture_bounded(4)
            .await;
        assert!(matches!(
            result,
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
