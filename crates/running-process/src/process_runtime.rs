//! Process-global Tokio runtime and actor command boundary for async processes.
//!
//! A process actor is the exclusive owner of one platform child. Public async
//! handles communicate only through commands, so later sync compatibility
//! adapters can block over the same engine without duplicating child state.

use std::io;
use std::process::{ExitStatus, Output};
use std::sync::OnceLock;

use running_process_platform_internal::{PlatformChild, SpawnSpec};
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
    serve_child(child, &mut commands).await;
}

async fn serve_child(child: PlatformChild, commands: &mut mpsc::Receiver<Command>) {
    let mut child = Some(child);
    while let Some(command) = commands.recv().await {
        match command {
            Command::Pid(reply) => {
                let _ = reply.send(
                    child
                        .as_ref()
                        .and_then(PlatformChild::id)
                        .ok_or(ProcessError::NotRunning),
                );
            }
            Command::Wait(reply) => {
                let result = match child.as_mut() {
                    Some(child) => child.wait().await,
                    None => Err(not_running_error()),
                };
                let _ = reply.send(result);
            }
            Command::Kill(reply) => {
                let result = match child.as_mut() {
                    Some(child) => child.kill().await,
                    None => Err(not_running_error()),
                };
                let _ = reply.send(result);
            }
            Command::Output(reply) => {
                let result = match child.take() {
                    Some(child) => child.wait_with_output().await,
                    None => Err(not_running_error()),
                };
                let _ = reply.send(result);
            }
            Command::WriteStdin { bytes, reply } => {
                let result = match child.as_mut() {
                    Some(child) => child.write_stdin(&bytes).await,
                    None => Err(not_running_error()),
                };
                let _ = reply.send(result);
            }
            Command::CloseStdin(reply) => {
                let result = match child.as_mut() {
                    Some(child) => {
                        child.close_stdin();
                        Ok(())
                    }
                    None => Err(not_running_error()),
                };
                let _ = reply.send(result);
            }
        }
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
    use super::{runtime, ActorProcess};
    use running_process_platform_internal::{shell_spec, StreamMode};

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
}
