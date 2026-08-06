//! Process-global Tokio runtime and actor command boundary for async processes.
//!
//! A process actor is the exclusive owner of one platform child. Public async
//! handles communicate only through commands, so later sync compatibility
//! adapters can block over the same engine without duplicating child state.

use std::io;
use std::process::{ExitStatus, Output};
use std::sync::OnceLock;

use running_process_platform_internal::{
    PlatformChild, PlatformLifecycle, PlatformOutput, PlatformStdin, SpawnSpec,
};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::{mpsc, oneshot};

use crate::ProcessError;

static PROCESS_RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Return the library-owned runtime used by process actors.
pub(crate) fn runtime() -> &'static Runtime {
    PROCESS_RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(2)
            .enable_io()
            .enable_time()
            .thread_name("running-process-actor")
            .build()
            .expect("process runtime must initialize")
    })
}

/// Command handle for one actor-owned process.
pub(crate) struct ActorProcess {
    commands: mpsc::Sender<Command>,
}

impl ActorProcess {
    /// Spawn a process actor and wait until the actor has attempted creation.
    pub(crate) async fn start(spec: SpawnSpec) -> Result<Self, ProcessError> {
        let (commands, receiver) = mpsc::channel(16);
        let (started_tx, started_rx) = oneshot::channel();
        runtime().spawn(run_actor(spec, receiver, started_tx));

        started_rx
            .await
            .map_err(|_| ProcessError::NotRunning)?
            .map_err(ProcessError::Spawn)?;
        Ok(Self { commands })
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
        self.send(Command::Output(reply_tx)).await?;
        reply_rx
            .await
            .map_err(|_| ProcessError::NotRunning)?
            .map_err(ProcessError::Io)
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
    Kill(oneshot::Sender<io::Result<()>>),
    Output(oneshot::Sender<io::Result<Output>>),
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
    serve_child(child, pid, &mut commands).await;
}

async fn serve_child(
    child: PlatformChild,
    pid: Option<u32>,
    commands: &mut mpsc::Receiver<Command>,
) {
    let (mut lifecycle, signal, mut stdin, mut stdout, mut stderr) = child.into_actor_parts();
    let mut exit_status = None;
    let mut waiters: Vec<oneshot::Sender<io::Result<ExitStatus>>> = Vec::new();
    let mut kill_waiters: Vec<oneshot::Sender<io::Result<()>>> = Vec::new();

    loop {
        let event = if exit_status.is_some() {
            ActorEvent::Command(commands.recv().await)
        } else {
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
            ActorEvent::Command(Some(Command::Kill(reply))) => {
                if exit_status.is_some() {
                    let _ = reply.send(Ok(()));
                } else if let Err(error) = signal.kill() {
                    let _ = reply.send(Err(error));
                } else {
                    kill_waiters.push(reply);
                }
            }
            ActorEvent::Command(Some(Command::Output(reply))) => {
                let result = capture_output(
                    &mut lifecycle,
                    &mut stdin,
                    &mut stdout,
                    &mut stderr,
                    exit_status,
                )
                .await;
                let _ = reply.send(result);
                return;
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
    Command(Option<Command>),
}

async fn capture_output(
    lifecycle: &mut PlatformLifecycle,
    stdin: &mut Option<PlatformStdin>,
    stdout: &mut Option<PlatformOutput>,
    stderr: &mut Option<PlatformOutput>,
    exit_status: Option<ExitStatus>,
) -> io::Result<Output> {
    drop(stdin.take());
    let stdout = read_output(stdout.take());
    let stderr = read_output(stderr.take());
    let (status, stdout, stderr) = match exit_status {
        Some(status) => {
            let (stdout, stderr) = tokio::try_join!(stdout, stderr)?;
            (status, stdout, stderr)
        }
        None => tokio::try_join!(lifecycle.wait(), stdout, stderr)?,
    };
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

async fn read_output(output: Option<PlatformOutput>) -> io::Result<Vec<u8>> {
    match output {
        Some(output) => output.read_to_end().await,
        None => Ok(Vec::new()),
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

    use super::{runtime, ActorProcess, Command};
    use running_process_platform_internal::{shell_spec, StreamMode};
    use tokio::sync::oneshot;

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

    #[tokio::test]
    async fn kill_is_delivered_while_an_actor_wait_is_pending() {
        #[cfg(windows)]
        let spec = shell_spec("ping -n 30 127.0.0.1 > nul")
            .stdin(StreamMode::Null)
            .stdout(StreamMode::Piped)
            .stderr(StreamMode::Piped);
        #[cfg(not(windows))]
        let spec = shell_spec("sleep 30")
            .stdin(StreamMode::Null)
            .stdout(StreamMode::Piped)
            .stderr(StreamMode::Piped);

        let process = ActorProcess::start(spec).await.expect("actor starts");
        let (wait_tx, wait_rx) = oneshot::channel();
        process
            .commands
            .send(Command::Wait(wait_tx))
            .await
            .expect("wait command is accepted");

        tokio::time::timeout(Duration::from_secs(2), process.kill())
            .await
            .expect("kill is not blocked by wait")
            .expect("kill succeeds");
        let status = tokio::time::timeout(Duration::from_secs(2), wait_rx)
            .await
            .expect("waiter is released")
            .expect("actor replies")
            .expect("wait succeeds");
        assert!(!status.success());
    }
}
