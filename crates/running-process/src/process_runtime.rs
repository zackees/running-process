//! Process-global Tokio runtime and actor command boundary for async processes.
//!
//! A process actor is the exclusive owner of one platform child. Public async
//! handles communicate only through commands, so later sync compatibility
//! adapters can block over the same engine without duplicating child state.

use std::io;
use std::process::{ExitStatus, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use running_process_platform_internal::{
    PlatformChild, PlatformLifecycle, PlatformOutput, PlatformStdin, SpawnSpec,
};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    AsyncProcessSessionChunk, AsyncProcessSessionEvent, AsyncProcessSessionOptions, ProcessError,
    SharedOutputCursor, SharedOutputLog, StreamKind,
};

static PROCESS_RUNTIME: OnceLock<Runtime> = OnceLock::new();
const DEFAULT_OUTPUT_LOG_CAPACITY: usize = 16 * 1024 * 1024;
// Tokio reserves the low three permit bits for internal bookkeeping. Passing
// a greater capacity to `mpsc::channel` panics instead of returning an error.
const MAX_MPSC_CAPACITY: usize = usize::MAX >> 3;

/// Return the library-owned runtime used by process actors.
pub(crate) fn runtime() -> &'static Runtime {
    PROCESS_RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(runtime_worker_threads())
            .enable_io()
            .enable_time()
            .thread_name("running-process-actor")
            .build()
            .expect("process runtime must initialize")
    })
}

/// Run one sync compatibility operation on the process-global actor runtime.
///
/// Blocking adapters deliberately reject calls made from an existing Tokio
/// runtime. Blocking that worker would deadlock actor progress, so callers in
/// async code must use the native async method instead.
pub(crate) fn block_on<F>(future: F) -> Result<F::Output, ProcessError>
where
    F: std::future::Future,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(ProcessError::RuntimeContext);
    }
    Ok(runtime().block_on(future))
}

fn runtime_worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(2)
        .clamp(2, 4)
}

/// Command handle for one actor-owned process.
pub(crate) struct ActorProcess {
    commands: mpsc::Sender<Command>,
    output_log: SharedOutputLog,
}

impl ActorProcess {
    /// Spawn a process actor and wait until the actor has attempted creation.
    pub(crate) async fn start(spec: SpawnSpec) -> Result<Self, ProcessError> {
        let (commands, receiver) = mpsc::channel(16);
        let (started_tx, started_rx) = oneshot::channel();
        let output_log = SharedOutputLog::new(DEFAULT_OUTPUT_LOG_CAPACITY);
        runtime().spawn(run_actor(spec, receiver, started_tx, output_log.clone()));

        started_rx
            .await
            .map_err(|_| ProcessError::NotRunning)?
            .map_err(ProcessError::Spawn)?;
        Ok(Self {
            commands,
            output_log,
        })
    }

    pub(crate) fn output_cursor(&self) -> SharedOutputCursor {
        self.output_log.cursor()
    }

    pub(crate) async fn pid(&self) -> Result<u32, ProcessError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(Command::Pid(reply_tx)).await?;
        reply_rx.await.map_err(|_| ProcessError::NotRunning)?
    }

    pub(crate) async fn wait(&self) -> Result<ExitStatus, ProcessError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(Command::Wait(reply_tx)).await?;
        reply_rx
            .await
            .map_err(|_| ProcessError::NotRunning)?
            .map_err(ProcessError::Io)
    }

    /// Report the exit status if the actor has already observed it.
    ///
    /// This never waits. While an output capture is in flight the actor is
    /// selecting on capture completion rather than the lifecycle handle, so
    /// exit is reported once that capture finishes.
    pub(crate) async fn poll(&self) -> Result<Option<ExitStatus>, ProcessError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(Command::Poll(reply_tx)).await?;
        reply_rx.await.map_err(|_| ProcessError::NotRunning)
    }

    /// Signal the child's process group to shut down gracefully.
    ///
    /// `Ok(false)` means the child has no group of its own, so there was
    /// nothing addressable to signal.
    pub(crate) async fn terminate_group_soft(&self) -> Result<bool, ProcessError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(Command::TerminateGroupSoft(reply_tx)).await?;
        reply_rx
            .await
            .map_err(|_| ProcessError::NotRunning)?
            .map_err(ProcessError::Io)
    }

    pub(crate) async fn kill(&self) -> Result<(), ProcessError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(Command::Kill(reply_tx)).await?;
        reply_rx
            .await
            .map_err(|_| ProcessError::NotRunning)?
            .map_err(ProcessError::Io)
    }

    pub(crate) async fn output(&self) -> Result<Output, ProcessError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(Command::Output {
            limit: None,
            reply: reply_tx,
        })
        .await?;
        reply_rx.await.map_err(|_| ProcessError::NotRunning)?
    }

    pub(crate) async fn output_bounded(&self, limit: usize) -> Result<Output, ProcessError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(Command::Output {
            limit: Some(limit),
            reply: reply_tx,
        })
        .await?;
        reply_rx.await.map_err(|_| ProcessError::NotRunning)?
    }

    pub(crate) async fn write_stdin(&self, bytes: Vec<u8>) -> Result<(), ProcessError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(Command::WriteStdin {
            bytes,
            reply: reply_tx,
        })
        .await?;
        reply_rx
            .await
            .map_err(|_| ProcessError::NotRunning)?
            .map_err(ProcessError::Io)
    }

    pub(crate) async fn close_stdin(&self) -> Result<(), ProcessError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(Command::CloseStdin(reply_tx)).await?;
        reply_rx
            .await
            .map_err(|_| ProcessError::NotRunning)?
            .map_err(ProcessError::Io)
    }

    async fn send(&self, command: Command) -> Result<(), ProcessError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| ProcessError::NotRunning)
    }
}

/// Terminal-owner command handle for the continuously pumped session actor.
///
/// Unlike [`ActorProcess`], this deliberately has no clone implementation:
/// dropping its only owner activates the configured terminal cleanup policy.
pub(crate) struct SessionProcess {
    commands: mpsc::Sender<SessionCommand>,
    stdin: Option<mpsc::Sender<SessionStdinRequest>>,
    owner_drop: Option<oneshot::Sender<()>>,
    exit_status: watch::Receiver<SessionExitState>,
    pid: u32,
    max_stdin_write: usize,
}

impl SessionProcess {
    pub(crate) async fn start(
        spec: SpawnSpec,
        options: AsyncProcessSessionOptions,
    ) -> Result<(Self, mpsc::Receiver<AsyncProcessSessionEvent>), ProcessError> {
        validate_session_options(options)?;

        let (commands, receiver) = mpsc::channel(options.max_queued_chunks);
        let (owner_drop, owner_drop_rx) = oneshot::channel();
        let (exit_tx, exit_status) = watch::channel(SessionExitState::Running);
        let (started_tx, started_rx) = oneshot::channel();
        let (output_tx, output_rx) = mpsc::channel(options.max_queued_chunks);
        runtime().spawn(run_session_actor(
            spec,
            options,
            receiver,
            owner_drop_rx,
            exit_tx,
            started_tx,
            output_tx,
        ));

        let started = started_rx
            .await
            .map_err(|_| ProcessError::NotRunning)?
            .map_err(ProcessError::Spawn)?;
        Ok((
            Self {
                commands,
                stdin: started.stdin,
                owner_drop: Some(owner_drop),
                exit_status,
                pid: started.pid,
                max_stdin_write: options.max_chunk_bytes,
            },
            output_rx,
        ))
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    pub(crate) async fn wait(&self) -> Result<ExitStatus, ProcessError> {
        let mut exit_status = self.exit_status.clone();
        loop {
            let state = { exit_status.borrow_and_update().clone() };
            match state {
                SessionExitState::Running => {
                    exit_status
                        .changed()
                        .await
                        .map_err(|_| ProcessError::NotRunning)?;
                }
                SessionExitState::Exited(status) => return Ok(status),
                SessionExitState::Failed(error) => return Err(ProcessError::Io(error.into_io())),
            }
        }
    }

    pub(crate) async fn poll(&self) -> Result<Option<ExitStatus>, ProcessError> {
        match self.exit_status.borrow().clone() {
            SessionExitState::Running => Ok(None),
            SessionExitState::Exited(status) => Ok(Some(status)),
            SessionExitState::Failed(error) => Err(ProcessError::Io(error.into_io())),
        }
    }

    pub(crate) async fn kill(&self) -> Result<(), ProcessError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(SessionCommand::Kill(reply_tx)).await?;
        reply_rx
            .await
            .map_err(|_| ProcessError::NotRunning)?
            .map_err(ProcessError::Io)?;
        self.wait().await.map(|_| ())
    }

    pub(crate) async fn terminate_group_soft(&self) -> Result<bool, ProcessError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(SessionCommand::TerminateGroupSoft(reply_tx))
            .await?;
        reply_rx
            .await
            .map_err(|_| ProcessError::NotRunning)?
            .map_err(ProcessError::Io)
    }

    pub(crate) async fn write_stdin(&self, bytes: Vec<u8>) -> Result<(), ProcessError> {
        if bytes.len() > self.max_stdin_write {
            return Err(ProcessError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session stdin write exceeds max_chunk_bytes",
            )));
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        let stdin = self.stdin.as_ref().ok_or(ProcessError::NotRunning)?;
        stdin
            .send(SessionStdinRequest {
                bytes,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ProcessError::NotRunning)?;
        reply_rx
            .await
            .map_err(|_| ProcessError::NotRunning)?
            .map_err(ProcessError::Io)
    }

    pub(crate) async fn close_stdin(&self) -> Result<(), ProcessError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(SessionCommand::CloseStdin(reply_tx)).await?;
        reply_rx
            .await
            .map_err(|_| ProcessError::NotRunning)?
            .map_err(ProcessError::Io)
    }

    pub(crate) async fn cpu_time(&self) -> Result<Option<Duration>, ProcessError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(SessionCommand::CpuTime(reply_tx)).await?;
        reply_rx
            .await
            .map_err(|_| ProcessError::NotRunning)?
            .map_err(ProcessError::Io)
    }

    async fn send(&self, command: SessionCommand) -> Result<(), ProcessError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| ProcessError::NotRunning)
    }
}

impl Drop for SessionProcess {
    fn drop(&mut self) {
        if let Some(owner_drop) = self.owner_drop.take() {
            // This zero-byte terminal signal is independent of bounded stdin
            // and output queues, so a full queue cannot orphan the child.
            let _ = owner_drop.send(());
        }
    }
}

enum SessionCommand {
    Kill(oneshot::Sender<io::Result<()>>),
    TerminateGroupSoft(oneshot::Sender<io::Result<bool>>),
    CloseStdin(oneshot::Sender<io::Result<()>>),
    CpuTime(oneshot::Sender<io::Result<Option<Duration>>>),
}

struct SessionStdinRequest {
    bytes: Vec<u8>,
    reply: oneshot::Sender<io::Result<()>>,
}

struct SessionStdinWorker {
    task: tokio::task::JoinHandle<()>,
}

struct SessionStarted {
    pid: u32,
    stdin: Option<mpsc::Sender<SessionStdinRequest>>,
}

#[derive(Clone)]
enum SessionExitState {
    Running,
    Exited(ExitStatus),
    Failed(SessionExitError),
}

#[derive(Clone)]
struct SessionExitError {
    kind: io::ErrorKind,
    message: Arc<str>,
}

impl SessionExitError {
    fn from_io(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: Arc::from(error.to_string()),
        }
    }

    fn into_io(self) -> io::Error {
        io::Error::new(self.kind, self.message.to_string())
    }
}

struct SessionPump {
    stream: StreamKind,
}

struct SessionPumpDone {
    stream: StreamKind,
}

/// State shared with output pumps after the direct lifecycle observes exit.
///
/// The public `None` policy needs an explicit state distinct from the period
/// before direct exit: both wait indefinitely, but only the former means a
/// descendant-held pipe is intentional rather than still being monitored for
/// the direct child's transition.
#[derive(Clone, Copy)]
enum PostExitPipeReadPolicy {
    BeforeDirectExit,
    WaitForEof,
    AbandonAfter(Duration),
}

struct SessionActor<'a> {
    lifecycle: PlatformLifecycle,
    signal: running_process_platform_internal::PlatformEmergencySignal,
    stdin_worker: Option<SessionStdinWorker>,
    stdout: Option<PlatformOutput>,
    stderr: Option<PlatformOutput>,
    options: AsyncProcessSessionOptions,
    commands: &'a mut mpsc::Receiver<SessionCommand>,
    owner_drop: &'a mut oneshot::Receiver<()>,
    exit_tx: watch::Sender<SessionExitState>,
    output_tx: mpsc::Sender<AsyncProcessSessionEvent>,
}

fn validate_session_options(options: AsyncProcessSessionOptions) -> Result<(), ProcessError> {
    if options.max_queued_chunks == 0 {
        return Err(invalid_session_options(
            "max_queued_chunks must be greater than zero",
        ));
    }
    if options.max_queued_chunks > MAX_MPSC_CAPACITY {
        return Err(invalid_session_options(
            "max_queued_chunks exceeds Tokio's bounded queue capacity",
        ));
    }
    if options.max_chunk_bytes == 0 {
        return Err(invalid_session_options(
            "max_chunk_bytes must be greater than zero",
        ));
    }
    if options
        .max_queued_chunks
        .checked_add(4)
        .and_then(|chunks| chunks.checked_mul(options.max_chunk_bytes))
        .is_none()
    {
        return Err(invalid_session_options(
            "session output byte bound overflows usize",
        ));
    }
    Ok(())
}

fn invalid_session_options(message: &'static str) -> ProcessError {
    ProcessError::Io(io::Error::new(io::ErrorKind::InvalidInput, message))
}

async fn run_session_actor(
    spec: SpawnSpec,
    options: AsyncProcessSessionOptions,
    mut commands: mpsc::Receiver<SessionCommand>,
    mut owner_drop: oneshot::Receiver<()>,
    exit_tx: watch::Sender<SessionExitState>,
    started: oneshot::Sender<io::Result<SessionStarted>>,
    output_tx: mpsc::Sender<AsyncProcessSessionEvent>,
) {
    let child = match spec.spawn().await {
        Ok(child) => child,
        Err(error) => {
            let _ = started.send(Err(error));
            return;
        }
    };
    let Some(pid) = child.id() else {
        let _ = started.send(Err(io::Error::other(
            "spawned child has no numeric identifier",
        )));
        return;
    };
    let (lifecycle, signal, stdin, stdout, stderr) = child.into_actor_parts();
    let (stdin, stdin_worker) = start_session_stdin(stdin, options.max_queued_chunks);
    let _ = started.send(Ok(SessionStarted { pid, stdin }));
    serve_session_child(SessionActor {
        lifecycle,
        signal,
        stdin_worker,
        stdout,
        stderr,
        options,
        commands: &mut commands,
        owner_drop: &mut owner_drop,
        exit_tx,
        output_tx,
    })
    .await;
}

async fn serve_session_child(actor: SessionActor<'_>) {
    let SessionActor {
        mut lifecycle,
        signal,
        mut stdin_worker,
        stdout,
        stderr,
        options,
        commands,
        owner_drop,
        exit_tx,
        output_tx,
    } = actor;
    let (pump_done_tx, mut pump_done_rx) = mpsc::unbounded_channel();
    let (post_exit_grace, post_exit_grace_rx) =
        watch::channel(PostExitPipeReadPolicy::BeforeDirectExit);
    let mut pumps = Vec::with_capacity(2);
    if let Some(stdout) = stdout {
        pumps.push(start_session_pump(
            stdout,
            StreamKind::Stdout,
            options.max_chunk_bytes,
            output_tx.clone(),
            pump_done_tx.clone(),
            post_exit_grace_rx.clone(),
        ));
    }
    if let Some(stderr) = stderr {
        pumps.push(start_session_pump(
            stderr,
            StreamKind::Stderr,
            options.max_chunk_bytes,
            output_tx.clone(),
            pump_done_tx.clone(),
            post_exit_grace_rx,
        ));
    }
    drop(pump_done_tx);

    let mut output_tx = Some(output_tx);
    if pumps.is_empty() {
        drop(output_tx.take());
    }
    let mut exit_status = None;
    let mut commands_open = true;
    let mut owner_drop_open = true;
    let mut owner_dropped = false;

    loop {
        if owner_dropped && exit_status.is_some() && pumps.is_empty() {
            return;
        }

        tokio::select! {
            result = lifecycle.wait(), if exit_status.is_none() => {
                match result {
                    Ok(status) => {
                        exit_status = Some(status);
                        let _ = exit_tx.send(SessionExitState::Exited(status));
                        post_exit_grace.send_replace(match options.post_exit_grace {
                            Some(grace) => PostExitPipeReadPolicy::AbandonAfter(grace),
                            None => PostExitPipeReadPolicy::WaitForEof,
                        });
                        if pumps.is_empty() {
                            drop(output_tx.take());
                        }
                    }
                    Err(error) => {
                        let _ = exit_tx.send(SessionExitState::Failed(SessionExitError::from_io(&error)));
                        return;
                    }
                }
            }
            command = commands.recv(), if commands_open => {
                match command {
                    None => {
                        commands_open = false;
                        owner_drop_open = false;
                        owner_dropped = true;
                        close_session_stdin(&mut stdin_worker);
                        if options.kill_on_drop && exit_status.is_none() {
                            let _ = start_session_kill(&signal, &mut lifecycle);
                        }
                    }
                    Some(SessionCommand::Kill(reply)) => {
                        if exit_status.is_some() {
                            let _ = reply.send(Ok(()));
                        } else {
                            match start_session_kill(&signal, &mut lifecycle) {
                                Ok(()) => { let _ = reply.send(Ok(())); }
                                Err(error) => { let _ = reply.send(Err(error)); }
                            }
                        }
                    }
                    Some(SessionCommand::TerminateGroupSoft(reply)) => {
                        if exit_status.is_some() {
                            let _ = reply.send(Ok(false));
                        } else {
                            let _ = reply.send(signal.terminate_group_soft());
                        }
                    }
                    Some(SessionCommand::CloseStdin(reply)) => {
                        close_session_stdin(&mut stdin_worker);
                        let _ = reply.send(Ok(()));
                    }
                    Some(SessionCommand::CpuTime(reply)) => {
                        let _ = reply.send(signal.cpu_time());
                    }
                }
            }
            _ = &mut *owner_drop, if owner_drop_open => {
                owner_drop_open = false;
                commands_open = false;
                owner_dropped = true;
                close_session_stdin(&mut stdin_worker);
                if options.kill_on_drop && exit_status.is_none() {
                    let _ = start_session_kill(&signal, &mut lifecycle);
                }
            }
            done = pump_done_rx.recv(), if !pumps.is_empty() => {
                if let Some(done) = done {
                    if let Some(index) = pumps.iter().position(|pump| pump.stream == done.stream) {
                        pumps.swap_remove(index);
                    }
                    if pumps.is_empty() {
                        drop(output_tx.take());
                    }
                }
            }
        }
    }
}

fn start_session_pump(
    output: PlatformOutput,
    stream: StreamKind,
    max_chunk_bytes: usize,
    output_tx: mpsc::Sender<AsyncProcessSessionEvent>,
    done_tx: mpsc::UnboundedSender<SessionPumpDone>,
    post_exit_grace: watch::Receiver<PostExitPipeReadPolicy>,
) -> SessionPump {
    runtime().spawn(async move {
        pump_session_output(output, stream, max_chunk_bytes, output_tx, post_exit_grace).await;
        let _ = done_tx.send(SessionPumpDone { stream });
    });
    SessionPump { stream }
}

async fn pump_session_output(
    mut output: PlatformOutput,
    stream: StreamKind,
    max_chunk_bytes: usize,
    output_tx: mpsc::Sender<AsyncProcessSessionEvent>,
    mut post_exit_grace: watch::Receiver<PostExitPipeReadPolicy>,
) {
    let mut bytes = vec![0_u8; max_chunk_bytes];
    let mut deliver = true;
    // This budget starts on the first read after direct exit, then counts
    // only time actually waiting on the pipe. Queue delivery deliberately
    // pauses it, so bounded consumer backpressure cannot lose readable data.
    let mut post_exit_read_budget = None;
    loop {
        match read_session_chunk(
            &mut output,
            &mut bytes,
            &mut post_exit_grace,
            &mut post_exit_read_budget,
        )
        .await
        {
            SessionRead::Abandoned => {
                // The timer only runs while an actual pipe read is pending.
                // It is therefore impossible to mistake queue backpressure
                // for a descendant holding the write end open.
                drop(output);
                if deliver {
                    let _ = output_tx
                        .send(AsyncProcessSessionEvent::StreamAbandoned(stream))
                        .await;
                }
                return;
            }
            SessionRead::Read(Err(error)) => {
                if deliver {
                    let _ = output_tx
                        .send(AsyncProcessSessionEvent::StreamError {
                            stream,
                            kind: error.kind(),
                            message: error.to_string(),
                            raw_os_error: error.raw_os_error(),
                        })
                        .await;
                }
                return;
            }
            SessionRead::Read(Ok(0)) => {
                if deliver {
                    let _ = output_tx
                        .send(AsyncProcessSessionEvent::StreamEof(stream))
                        .await;
                }
                return;
            }
            SessionRead::Read(Ok(size)) => {
                if deliver {
                    let event = AsyncProcessSessionEvent::Chunk(AsyncProcessSessionChunk {
                        stream,
                        bytes: bytes[..size].to_vec(),
                    });
                    if output_tx.send(event).await.is_err() {
                        // A caller may explicitly detach while retaining
                        // `kill_on_drop = false`. Keep draining this pipe so
                        // the direct child can progress and be reaped.
                        deliver = false;
                    }
                }
            }
        }
    }
}

enum SessionRead {
    Read(io::Result<usize>),
    Abandoned,
}

async fn read_session_chunk(
    output: &mut PlatformOutput,
    bytes: &mut [u8],
    post_exit_grace: &mut watch::Receiver<PostExitPipeReadPolicy>,
    post_exit_read_budget: &mut Option<Duration>,
) -> SessionRead {
    loop {
        let policy = { *post_exit_grace.borrow_and_update() };
        match policy {
            PostExitPipeReadPolicy::AbandonAfter(grace) => {
                let remaining = *post_exit_read_budget.get_or_insert(grace);
                let read_started = tokio::time::Instant::now();
                match read_before_post_exit_abandon(output.read_chunk(bytes), remaining).await {
                    SessionRead::Read(result) => {
                        *post_exit_read_budget =
                            Some(remaining.saturating_sub(read_started.elapsed()));
                        return SessionRead::Read(result);
                    }
                    SessionRead::Abandoned => return SessionRead::Abandoned,
                }
            }
            PostExitPipeReadPolicy::WaitForEof => {
                return SessionRead::Read(output.read_chunk(bytes).await);
            }
            PostExitPipeReadPolicy::BeforeDirectExit => {
                tokio::select! {
                    result = output.read_chunk(bytes) => return SessionRead::Read(result),
                    changed = post_exit_grace.changed() => {
                        if changed.is_err() {
                            return SessionRead::Read(output.read_chunk(bytes).await);
                        }
                    }
                }
            }
        }
    }
}

/// Await a pipe read until the cumulative post-exit read budget expires.
///
/// At the exact expiry boundary both futures can be ready. Buffered pipe data
/// is still observable and must win: the grace only abandons a genuinely
/// pending read, never bytes the kernel already made available.
async fn read_before_post_exit_abandon<F>(read: F, remaining: Duration) -> SessionRead
where
    F: std::future::Future<Output = io::Result<usize>>,
{
    tokio::select! {
        biased;
        result = read => SessionRead::Read(result),
        _ = tokio::time::sleep(remaining) => SessionRead::Abandoned,
    }
}

fn start_session_stdin(
    stdin: Option<PlatformStdin>,
    queue_capacity: usize,
) -> (
    Option<mpsc::Sender<SessionStdinRequest>>,
    Option<SessionStdinWorker>,
) {
    let Some(mut stdin) = stdin else {
        return (None, None);
    };
    let (sender, mut receiver) = mpsc::channel::<SessionStdinRequest>(queue_capacity);
    let task = runtime().spawn(async move {
        while let Some(request) = receiver.recv().await {
            let result = stdin.write(&request.bytes).await;
            let _ = request.reply.send(result);
        }
    });
    (Some(sender), Some(SessionStdinWorker { task }))
}

fn close_session_stdin(worker: &mut Option<SessionStdinWorker>) {
    if let Some(worker) = worker.take() {
        // Dropping the task owns and closes the pipe immediately. It does not
        // wait behind a blocked child read, so lifecycle commands stay live.
        worker.task.abort();
    }
}

fn start_session_kill(
    signal: &running_process_platform_internal::PlatformEmergencySignal,
    lifecycle: &mut PlatformLifecycle,
) -> io::Result<()> {
    match signal.kill() {
        Ok(()) => Ok(()),
        // No launch-bound out-of-band signal is not a reason to abandon the
        // actor-owned child handle. `start_kill` uses that owned handle rather
        // than a cached PID, including on pidfd-restricted Linux hosts.
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::Unsupported | io::ErrorKind::BrokenPipe
            ) =>
        {
            lifecycle.start_kill()
        }
        Err(error) => Err(error),
    }
}

enum Command {
    Pid(oneshot::Sender<Result<u32, ProcessError>>),
    Wait(oneshot::Sender<io::Result<ExitStatus>>),
    Poll(oneshot::Sender<Option<ExitStatus>>),
    Kill(oneshot::Sender<io::Result<()>>),
    TerminateGroupSoft(oneshot::Sender<io::Result<bool>>),
    Output {
        limit: Option<usize>,
        reply: oneshot::Sender<Result<Output, ProcessError>>,
    },
    WriteStdin {
        bytes: Vec<u8>,
        reply: oneshot::Sender<io::Result<()>>,
    },
    CloseStdin(oneshot::Sender<io::Result<()>>),
}

async fn run_actor(
    spec: SpawnSpec,
    mut commands: mpsc::Receiver<Command>,
    started: oneshot::Sender<io::Result<()>>,
    output_log: SharedOutputLog,
) {
    let child = match spec.spawn().await {
        Ok(child) => {
            let _ = started.send(Ok(()));
            child
        }
        Err(error) => {
            let _ = started.send(Err(error));
            return;
        }
    };
    let pid = child.id();
    serve_child(child, pid, &mut commands, output_log).await;
}

async fn serve_child(
    child: PlatformChild,
    pid: Option<u32>,
    commands: &mut mpsc::Receiver<Command>,
    output_log: SharedOutputLog,
) {
    let (lifecycle, signal, mut stdin, mut stdout, mut stderr) = child.into_actor_parts();
    let mut lifecycle = Some(lifecycle);
    let mut exit_status = None;
    let mut waiters: Vec<oneshot::Sender<io::Result<ExitStatus>>> = Vec::new();
    let mut kill_waiters: Vec<oneshot::Sender<io::Result<()>>> = Vec::new();
    let mut capture_completion: Option<oneshot::Receiver<Result<Output, CaptureError>>> = None;
    let mut capture_reply: Option<oneshot::Sender<Result<Output, ProcessError>>> = None;
    let mut capture_kill: Option<mpsc::UnboundedSender<oneshot::Sender<io::Result<()>>>> = None;

    loop {
        let event = if let Some(completion) = capture_completion.as_mut() {
            tokio::select! {
                result = completion => ActorEvent::Capture(result),
                command = commands.recv() => ActorEvent::Command(command),
            }
        } else if exit_status.is_some() {
            ActorEvent::Command(commands.recv().await)
        } else {
            let lifecycle = lifecycle
                .as_mut()
                .expect("live actor retains its lifecycle capability");
            tokio::select! {
                result = lifecycle.wait() => ActorEvent::Exit(result),
                command = commands.recv() => ActorEvent::Command(command),
            }
        };

        match event {
            ActorEvent::Exit(Ok(status)) => {
                for waiter in waiters.drain(..) {
                    let _ = waiter.send(Ok(status));
                }
                for waiter in kill_waiters.drain(..) {
                    let _ = waiter.send(Ok(()));
                }
                exit_status = Some(status);
            }
            ActorEvent::Exit(Err(error)) => {
                for waiter in waiters.drain(..) {
                    let _ = waiter.send(Err(io::Error::new(error.kind(), error.to_string())));
                }
                for waiter in kill_waiters.drain(..) {
                    let _ = waiter.send(Err(io::Error::new(error.kind(), error.to_string())));
                }
                return;
            }
            ActorEvent::Capture(Ok(Ok(output))) => {
                let status = output.status;
                if let Some(reply) = capture_reply.take() {
                    let _ = reply.send(Ok(output));
                }
                for waiter in waiters.drain(..) {
                    let _ = waiter.send(Ok(status));
                }
                for waiter in kill_waiters.drain(..) {
                    let _ = waiter.send(Ok(()));
                }
                return;
            }
            ActorEvent::Capture(Ok(Err(error))) => {
                let process_error = error.into_process_error();
                if let Some(reply) = capture_reply.take() {
                    let _ = reply.send(Err(process_error));
                }
                let error = io::Error::other("async output capture failed");
                for waiter in waiters.drain(..) {
                    let _ = waiter.send(Err(io::Error::new(error.kind(), error.to_string())));
                }
                for waiter in kill_waiters.drain(..) {
                    let _ = waiter.send(Err(io::Error::new(error.kind(), error.to_string())));
                }
                return;
            }
            ActorEvent::Capture(Err(_)) => {
                if let Some(reply) = capture_reply.take() {
                    let _ = reply.send(Err(ProcessError::NotRunning));
                }
                for waiter in waiters.drain(..) {
                    let _ = waiter.send(Err(not_running_error()));
                }
                for waiter in kill_waiters.drain(..) {
                    let _ = waiter.send(Err(not_running_error()));
                }
                return;
            }
            ActorEvent::Command(None) => return,
            ActorEvent::Command(Some(Command::Pid(reply))) => {
                let _ = reply.send(pid.ok_or(ProcessError::NotRunning));
            }
            ActorEvent::Command(Some(Command::Wait(reply))) => {
                if let Some(status) = exit_status {
                    let _ = reply.send(Ok(status));
                } else {
                    waiters.push(reply);
                }
            }
            ActorEvent::Command(Some(Command::Poll(reply))) => {
                let _ = reply.send(exit_status);
            }
            ActorEvent::Command(Some(Command::TerminateGroupSoft(reply))) => {
                if exit_status.is_some() {
                    // Nothing left to ask nicely. Report "no group signalled"
                    // rather than an error: the child is already gone.
                    let _ = reply.send(Ok(false));
                } else {
                    // AsyncProcess predates session identity capabilities and
                    // retains its macOS raw child-group compatibility path.
                    // AsyncProcessSession uses the identity-safe method in
                    // its separate actor loop above.
                    let _ = reply.send(signal.terminate_group_soft_legacy());
                }
            }
            ActorEvent::Command(Some(Command::Kill(reply))) => {
                if exit_status.is_some() {
                    let _ = reply.send(Ok(()));
                } else if let Some(capture_kill) = capture_kill.as_ref() {
                    // The capture task still owns the direct child handle.
                    // Forward to that owner so pidfd-restricted hosts retain
                    // an identity-safe direct kill while pipes are draining.
                    if let Err(error) = capture_kill.send(reply) {
                        let _ = error.0.send(Err(not_running_error()));
                    }
                } else {
                    let result = match signal.kill() {
                        Ok(()) => Ok(()),
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::Unsupported | io::ErrorKind::BrokenPipe
                            ) =>
                        {
                            lifecycle
                                .as_mut()
                                .ok_or_else(not_running_error)
                                .and_then(PlatformLifecycle::start_kill)
                        }
                        Err(error) => Err(error),
                    };
                    match result {
                        Ok(()) => kill_waiters.push(reply),
                        Err(error) => {
                            let _ = reply.send(Err(error));
                        }
                    }
                }
            }
            ActorEvent::Command(Some(Command::Output { limit, reply })) => {
                if capture_completion.is_some() {
                    let _ = reply.send(Err(ProcessError::NotRunning));
                    continue;
                }

                // Capture owns the lifecycle and both output endpoints in a
                // task on the process-global runtime. The actor retains the
                // emergency signal and command receiver, so kill and queued
                // waits remain responsive while pipes drain.
                drop(stdin.take());
                let lifecycle = lifecycle
                    .take()
                    .expect("capture starts with the lifecycle capability");
                let stdout = stdout.take();
                let stderr = stderr.take();
                let known_exit_status = exit_status;
                let capture_log = output_log.clone();
                let completion_log = output_log.clone();
                let (capture_tx, capture_rx) = oneshot::channel();
                let (capture_kill_tx, capture_kill_rx) = mpsc::unbounded_channel();
                runtime().spawn(async move {
                    let result = capture_output(
                        lifecycle,
                        stdout,
                        stderr,
                        known_exit_status,
                        limit,
                        capture_log,
                        capture_kill_rx,
                    )
                    .await;
                    completion_log.close();
                    let _ = capture_tx.send(result);
                });
                capture_completion = Some(capture_rx);
                capture_reply = Some(reply);
                capture_kill = Some(capture_kill_tx);
            }
            ActorEvent::Command(Some(Command::WriteStdin { bytes, reply })) => {
                let result = match stdin.as_mut() {
                    Some(stdin) => stdin.write(&bytes).await,
                    None => Err(not_running_error()),
                };
                let _ = reply.send(result);
            }
            ActorEvent::Command(Some(Command::CloseStdin(reply))) => {
                drop(stdin.take());
                let _ = reply.send(Ok(()));
            }
        }
    }
}

enum ActorEvent {
    Exit(io::Result<ExitStatus>),
    Capture(Result<Result<Output, CaptureError>, oneshot::error::RecvError>),
    Command(Option<Command>),
}

enum CaptureError {
    Io(io::Error),
    Limit(usize),
}

impl CaptureError {
    fn into_process_error(self) -> ProcessError {
        match self {
            Self::Io(error) => ProcessError::Io(error),
            Self::Limit(limit) => ProcessError::OutputLimitExceeded { limit },
        }
    }
}

async fn capture_output(
    mut lifecycle: PlatformLifecycle,
    stdout: Option<PlatformOutput>,
    stderr: Option<PlatformOutput>,
    exit_status: Option<ExitStatus>,
    limit: Option<usize>,
    output_log: SharedOutputLog,
    mut kill_requests: mpsc::UnboundedReceiver<oneshot::Sender<io::Result<()>>>,
) -> Result<Output, CaptureError> {
    let budget = limit.map(|limit| Arc::new(CaptureBudget::new(limit)));
    let stdout = runtime().spawn(read_output(
        stdout,
        budget.clone(),
        output_log.clone(),
        StreamKind::Stdout,
    ));
    let stderr = runtime().spawn(read_output(stderr, budget, output_log, StreamKind::Stderr));
    let mut kill_replies = Vec::new();
    let mut kill_requests_open = true;
    let status = match exit_status {
        Some(status) => Ok(status),
        None => loop {
            tokio::select! {
                status = lifecycle.wait() => break status,
                request = kill_requests.recv(), if kill_requests_open => match request {
                    Some(reply) => match lifecycle.start_kill() {
                        Ok(()) => kill_replies.push(reply),
                        Err(error) => { let _ = reply.send(Err(error)); }
                    },
                    None => kill_requests_open = false,
                }
            }
        },
    };
    let status = status.map_err(CaptureError::Io)?;
    for reply in kill_replies {
        let _ = reply.send(Ok(()));
    }
    let stdout = stdout
        .await
        .map_err(|error| CaptureError::Io(io::Error::other(error.to_string())))??;
    let stderr = stderr
        .await
        .map_err(|error| CaptureError::Io(io::Error::other(error.to_string())))??;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

struct CaptureBudget {
    limit: usize,
    used: AtomicUsize,
}

impl CaptureBudget {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            used: AtomicUsize::new(0),
        }
    }
}

async fn read_output(
    output: Option<PlatformOutput>,
    budget: Option<Arc<CaptureBudget>>,
    output_log: SharedOutputLog,
    stream: StreamKind,
) -> Result<Vec<u8>, CaptureError> {
    let Some(mut output) = output else {
        return Ok(Vec::new());
    };
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut overflowed = false;
    loop {
        let size = output
            .read_chunk(&mut chunk)
            .await
            .map_err(CaptureError::Io)?;
        if size == 0 {
            break;
        }
        output_log.append(stream, chunk[..size].to_vec());
        if overflowed {
            continue;
        }
        if let Some(budget) = &budget {
            let used = budget.used.fetch_add(size, Ordering::AcqRel);
            if used.saturating_add(size) > budget.limit {
                overflowed = true;
                continue;
            }
        }
        bytes.extend_from_slice(&chunk[..size]);
    }
    if let Some(budget) = budget.filter(|_| overflowed) {
        Err(CaptureError::Limit(budget.limit))
    } else {
        Ok(bytes)
    }
}

fn not_running_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "process actor no longer owns a child",
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        capture_output, runtime, runtime_worker_threads, ActorProcess, Command,
        DEFAULT_OUTPUT_LOG_CAPACITY,
    };
    use crate::SharedOutputLog;
    use running_process_platform_internal::{shell_spec, SpawnSpec, StreamMode};
    use tokio::sync::{mpsc, oneshot};

    #[test]
    fn process_runtime_worker_count_is_bounded() {
        assert!((2..=4).contains(&runtime_worker_threads()));
    }

    #[tokio::test]
    async fn actors_share_the_process_global_runtime() {
        let spec = shell_spec("exit 0")
            .stdin(StreamMode::Null)
            .stdout(StreamMode::Piped)
            .stderr(StreamMode::Piped);
        let process = ActorProcess::start(spec).await.expect("actor starts");
        assert_eq!(runtime().handle().id(), runtime().handle().id());
        assert!(process
            .output()
            .await
            .expect("actor output")
            .status
            .success());
    }

    /// A long-lived child spawned *without* a shell.
    ///
    /// `shell_spec` was the obvious choice and the wrong one: whether
    /// `/bin/sh -c "sleep 300"` execs `sleep` or forks it is shell- and
    /// image-dependent. When it forks, killing the shell leaves the grandchild
    /// holding the inherited stdout/stderr pipes, so capture never sees EOF.
    /// The capture kill lane must still acknowledge once it reaps the direct
    /// child; otherwise that retained pipe turns a kill into a hang. Exec'ing
    /// the sleeper directly removes the ambiguity.
    ///
    /// 300s rather than 30s so the bound below has an order of magnitude of
    /// headroom before "the child exited by itself" could be mistaken for
    /// "the kill worked".
    fn long_lived_piped_child() -> SpawnSpec {
        #[cfg(windows)]
        let spec = SpawnSpec::new("ping").arg("-n").arg("300").arg("127.0.0.1");
        #[cfg(not(windows))]
        let spec = SpawnSpec::new("sleep").arg("300");
        spec.stdout(StreamMode::Piped).stderr(StreamMode::Piped)
    }

    /// How long a kill may take before the test calls it blocked.
    ///
    /// The bound has to sit between "kill was delivered" and "the child just
    /// exited on its own", or the test proves nothing either way. These
    /// complete in ~7ms in the normal test job, but under coverage
    /// instrumentation the capture task -- which must reap the direct child
    /// before it acknowledges a kill, even while output keeps draining --
    /// competes for the same small shared runtime as every other test in the
    /// binary, and 2s was not enough headroom.
    /// Widening alone was not either: a 10s bound still timed out. So the
    /// children now live 300s instead of 30s, which buys this 30s bound a 10x
    /// margin while keeping "the child exited by itself" far out of reach. A
    /// failure here is now a real hang, not a slow runner.
    const NOT_BLOCKED: Duration = Duration::from_secs(30);

    #[tokio::test]
    async fn kill_is_delivered_while_an_actor_wait_is_pending() {
        let spec = long_lived_piped_child().stdin(StreamMode::Null);

        let process = ActorProcess::start(spec).await.expect("actor starts");
        let (wait_tx, wait_rx) = oneshot::channel();
        process
            .commands
            .send(Command::Wait(wait_tx))
            .await
            .expect("wait command is accepted");

        tokio::time::timeout(NOT_BLOCKED, process.kill())
            .await
            .expect("kill is not blocked by wait")
            .expect("kill succeeds");
        let status = tokio::time::timeout(NOT_BLOCKED, wait_rx)
            .await
            .expect("waiter is released")
            .expect("actor replies")
            .expect("wait succeeds");
        assert!(!status.success());
    }

    #[tokio::test]
    async fn kill_is_delivered_while_output_is_draining() {
        // The original coverage-only failure became easier to reproduce as
        // more tests shared this runtime. Keep several captures pending at
        // once so this regression exercises that scheduler/pipe pressure,
        // rather than proving only the unloaded single-child case.
        const CONCURRENT_CAPTURES: usize = 8;
        let mut pending = Vec::with_capacity(CONCURRENT_CAPTURES);
        for _ in 0..CONCURRENT_CAPTURES {
            let spec = long_lived_piped_child().stdin(StreamMode::Piped);
            let process = ActorProcess::start(spec).await.expect("actor starts");
            let (output_tx, output_rx) = oneshot::channel();
            process
                .commands
                .send(Command::Output {
                    limit: None,
                    reply: output_tx,
                })
                .await
                .expect("output command is accepted");
            pending.push((process, output_rx));
        }

        tokio::time::timeout(NOT_BLOCKED, async {
            for (process, _) in &pending {
                process.kill().await.expect("kill succeeds");
            }
        })
        .await
        .expect("kills are not blocked by concurrent output capture");

        tokio::time::timeout(NOT_BLOCKED, async {
            for (_, output_rx) in pending {
                let output = output_rx
                    .await
                    .expect("actor replies")
                    .expect("capture succeeds");
                assert!(!output.status.success());
            }
        })
        .await
        .expect("all output captures complete");
    }

    #[tokio::test]
    async fn closed_capture_kill_channel_does_not_starve_lifecycle_reap() {
        let child = shell_spec("exit 0")
            .stdout(StreamMode::Piped)
            .stderr(StreamMode::Piped)
            .spawn()
            .await
            .expect("spawn capture child");
        let (lifecycle, _signal, _stdin, stdout, stderr) = child.into_actor_parts();
        let (kill_tx, kill_rx) = mpsc::unbounded_channel();
        drop(kill_tx);

        let capture = tokio::time::timeout(
            Duration::from_secs(1),
            capture_output(
                lifecycle,
                stdout,
                stderr,
                None,
                None,
                SharedOutputLog::new(DEFAULT_OUTPUT_LOG_CAPACITY),
                kill_rx,
            ),
        )
        .await
        .expect("closed kill channel cannot hot-loop");
        let output = match capture {
            Ok(output) => output,
            Err(_) => panic!("capture completes"),
        };
        assert!(output.status.success());
    }

    #[tokio::test]
    async fn buffered_output_wins_zero_post_exit_grace() {
        let result =
            super::read_before_post_exit_abandon(std::future::ready(Ok(3)), Duration::ZERO).await;
        assert!(matches!(result, super::SessionRead::Read(Ok(3))));
    }

    #[tokio::test(start_paused = true)]
    async fn buffered_output_wins_at_post_exit_grace_expiry_boundary() {
        let read_ready_at_deadline = tokio::time::sleep(Duration::from_secs(1));
        let task = tokio::spawn(super::read_before_post_exit_abandon(
            async move {
                read_ready_at_deadline.await;
                Ok(7)
            },
            Duration::from_secs(1),
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;

        let result = task.await.expect("read/grace task joins");
        assert!(matches!(result, super::SessionRead::Read(Ok(7))));
    }
}
