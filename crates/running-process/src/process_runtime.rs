//! Process-global Tokio runtime and actor command boundary for async processes.
//!
//! A process actor is the exclusive owner of one platform child. Public async
//! handles communicate only through commands, so later sync compatibility
//! adapters can block over the same engine without duplicating child state.

use std::io;
use std::process::{ExitStatus, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use running_process_platform_internal::{
    PlatformChild, PlatformLifecycle, PlatformOutput, SpawnSpec,
};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::{mpsc, oneshot};

use crate::{ProcessError, SharedOutputCursor, SharedOutputLog, StreamKind};

static PROCESS_RUNTIME: OnceLock<Runtime> = OnceLock::new();
const DEFAULT_OUTPUT_LOG_CAPACITY: usize = 16 * 1024 * 1024;

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
                    let _ = reply.send(signal.terminate_group_soft());
                }
            }
            ActorEvent::Command(Some(Command::Kill(reply))) => {
                if exit_status.is_some() {
                    let _ = reply.send(Ok(()));
                } else if let Err(error) = signal.kill() {
                    let _ = reply.send(Err(error));
                } else {
                    kill_waiters.push(reply);
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
                runtime().spawn(async move {
                    let result = capture_output(
                        lifecycle,
                        stdout,
                        stderr,
                        known_exit_status,
                        limit,
                        capture_log,
                    )
                    .await;
                    completion_log.close();
                    let _ = capture_tx.send(result);
                });
                capture_completion = Some(capture_rx);
                capture_reply = Some(reply);
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
) -> Result<Output, CaptureError> {
    let budget = limit.map(|limit| Arc::new(CaptureBudget::new(limit)));
    let stdout = read_output(
        stdout,
        budget.clone(),
        output_log.clone(),
        StreamKind::Stdout,
    );
    let stderr = read_output(stderr, budget, output_log, StreamKind::Stderr);
    let (status, stdout, stderr) = match exit_status {
        Some(status) => {
            let (stdout, stderr) = tokio::join!(stdout, stderr);
            (Ok(status), stdout, stderr)
        }
        None => tokio::join!(lifecycle.wait(), stdout, stderr),
    };
    let status = status.map_err(CaptureError::Io)?;
    let stdout = stdout?;
    let stderr = stderr?;
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

    use super::{runtime, runtime_worker_threads, ActorProcess, Command};
    use running_process_platform_internal::{shell_spec, SpawnSpec, StreamMode};
    use tokio::sync::oneshot;

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
    /// holding the inherited stdout/stderr pipes, capture never sees EOF, and
    /// the kill acknowledgement -- which waits on capture -- never arrives.
    /// That is a hang the test would report as its own timeout. Exec'ing the
    /// sleeper directly removes the ambiguity.
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
    /// instrumentation the capture task -- whose completion is what
    /// acknowledges a kill -- competes for the same small shared runtime as
    /// every other test in the binary, and 2s was not enough headroom.
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
}
