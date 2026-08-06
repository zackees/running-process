//! Blessed asynchronous process operations.
//!
//! This crate is intentionally published as an implementation detail. It is
//! the only production owner of the Tokio process primitives used by the
//! async process API. Higher layers receive typed operations and never name
//! `tokio::process::Command` directly.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;
use std::process::{ExitStatus, Output, Stdio};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

/// Stdio policy for one child stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamMode {
    /// Leave the stream connected to the parent process.
    Inherit,
    /// Create an asynchronous pipe owned by the child handle.
    Piped,
    /// Connect the stream to the platform null device.
    Null,
}

impl StreamMode {
    fn apply(self) -> Stdio {
        match self {
            Self::Inherit => Stdio::inherit(),
            Self::Piped => Stdio::piped(),
            Self::Null => Stdio::null(),
        }
    }
}

/// Typed spawn description accepted by the blessed process boundary.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    program: OsString,
    args: Vec<OsString>,
    current_dir: Option<PathBuf>,
    env: Vec<(OsString, OsString)>,
    clear_env: bool,
    stdin: StreamMode,
    stdout: StreamMode,
    stderr: StreamMode,
}

impl SpawnSpec {
    /// Create a direct (non-shell) command description.
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            env: Vec::new(),
            clear_env: false,
            stdin: StreamMode::Inherit,
            stdout: StreamMode::Inherit,
            stderr: StreamMode::Inherit,
        }
    }

    /// Append one argument without requiring UTF-8.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Set the child working directory.
    pub fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }

    /// Add an environment override.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Start with an empty inherited environment before applying overrides.
    pub fn clear_env(mut self, clear: bool) -> Self {
        self.clear_env = clear;
        self
    }

    /// Configure child stdin.
    pub fn stdin(mut self, mode: StreamMode) -> Self {
        self.stdin = mode;
        self
    }

    /// Configure child stdout.
    pub fn stdout(mut self, mode: StreamMode) -> Self {
        self.stdout = mode;
        self
    }

    /// Configure child stderr.
    pub fn stderr(mut self, mode: StreamMode) -> Self {
        self.stderr = mode;
        self
    }

    /// Spawn using the canonical asynchronous platform operation.
    pub async fn spawn(self) -> io::Result<PlatformChild> {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        if let Some(current_dir) = self.current_dir.as_deref() {
            command.current_dir(current_dir);
        }
        if self.clear_env {
            command.env_clear();
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
            .stdin(self.stdin.apply())
            .stdout(self.stdout.apply())
            .stderr(self.stderr.apply());
        command.spawn().map(PlatformChild::new)
    }
}

/// Owned child handle returned by [`SpawnSpec::spawn`].
pub struct PlatformChild {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    signal: PlatformEmergencySignal,
}

impl PlatformChild {
    fn new(mut child: Child) -> Self {
        let signal = PlatformEmergencySignal { pid: child.id() };
        Self {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            child,
            signal,
        }
    }

    /// Return the operating-system process identifier, if available.
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Wait for completion without capturing output.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    /// Terminate the child and wait for its exit.
    pub async fn kill(&mut self) -> io::Result<()> {
        self.child.kill().await
    }

    /// Capture piped stdout and stderr while waiting for the child.
    pub async fn wait_with_output(self) -> io::Result<Output> {
        let Self {
            mut child,
            stdin,
            stdout,
            stderr,
            ..
        } = self;
        // Match Tokio's `Child::wait_with_output` contract: one-shot output
        // closes an owned stdin pipe so a child waiting for EOF can finish.
        drop(stdin);
        let (status, stdout, stderr) = tokio::try_join!(
            child.wait(),
            read_owned_to_end(stdout),
            read_owned_to_end(stderr),
        )?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    /// Write bytes to piped stdin and flush them.
    pub async fn write_stdin(&mut self, bytes: &[u8]) -> io::Result<()> {
        let stdin = self.stdin.as_mut().ok_or_else(stdin_not_piped)?;
        stdin.write_all(bytes).await?;
        stdin.flush().await
    }

    /// Close the piped stdin handle, delivering EOF to the child.
    ///
    /// This operation is idempotent. Closing an inherited or null stdin is
    /// also a no-op because there is no owned pipe to close.
    pub fn close_stdin(&mut self) {
        drop(self.stdin.take());
    }

    /// Read all bytes from piped stdout without waiting for process exit.
    pub async fn read_stdout_to_end(&mut self) -> io::Result<Vec<u8>> {
        let stdout = self.stdout.as_mut().ok_or_else(stdout_not_piped)?;
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await?;
        Ok(bytes)
    }

    /// Read all bytes from piped stderr without waiting for process exit.
    pub async fn read_stderr_to_end(&mut self) -> io::Result<Vec<u8>> {
        let stderr = self.stderr.as_mut().ok_or_else(stderr_not_piped)?;
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await?;
        Ok(bytes)
    }

    /// Split this child into sealed actor capabilities.
    ///
    /// The lifecycle wait handle, emergency termination handle, input pipe,
    /// and output readers are deliberately separate so the actor can keep
    /// accepting control commands while an asynchronous exit wait is pending.
    pub fn into_actor_parts(
        self,
    ) -> (
        PlatformLifecycle,
        PlatformEmergencySignal,
        Option<PlatformStdin>,
        Option<PlatformOutput>,
        Option<PlatformOutput>,
    ) {
        (
            PlatformLifecycle { child: self.child },
            self.signal,
            self.stdin.map(|stdin| PlatformStdin { stdin }),
            self.stdout.map(PlatformOutput::stdout),
            self.stderr.map(PlatformOutput::stderr),
        )
    }
}

/// Opaque exit-wait capability owned by a process actor.
pub struct PlatformLifecycle {
    child: Child,
}

impl PlatformLifecycle {
    /// Wait asynchronously for the child to exit.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }
}

/// Opaque, non-reap-capable emergency termination capability.
///
/// It can be used while the actor has a pending wait on
/// [`PlatformLifecycle`], but it cannot observe or consume the exit result.
pub struct PlatformEmergencySignal {
    pid: Option<u32>,
}

impl PlatformEmergencySignal {
    /// Request immediate termination without waiting for process reaping.
    pub fn kill(&self) -> io::Result<()> {
        let pid = self.pid.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "child process no longer has an emergency signal target",
            )
        })?;
        signal_process(pid)
    }
}

/// Opaque piped stdin capability owned by a process actor.
pub struct PlatformStdin {
    stdin: ChildStdin,
}

impl PlatformStdin {
    /// Write and flush bytes to the child stdin pipe.
    pub async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stdin.write_all(bytes).await?;
        self.stdin.flush().await
    }
}

/// Opaque stdout or stderr reader owned by a process actor.
pub struct PlatformOutput {
    reader: OutputReader,
}

enum OutputReader {
    Stdout(ChildStdout),
    Stderr(ChildStderr),
}

impl PlatformOutput {
    fn stdout(stdout: ChildStdout) -> Self {
        Self {
            reader: OutputReader::Stdout(stdout),
        }
    }

    fn stderr(stderr: ChildStderr) -> Self {
        Self {
            reader: OutputReader::Stderr(stderr),
        }
    }

    /// Drain this output endpoint to EOF without blocking a runtime worker.
    pub async fn read_to_end(self) -> io::Result<Vec<u8>> {
        match self.reader {
            OutputReader::Stdout(stdout) => read_owned_to_end(Some(stdout)).await,
            OutputReader::Stderr(stderr) => read_owned_to_end(Some(stderr)).await,
        }
    }

    /// Read the next asynchronous chunk from this output endpoint.
    ///
    /// The caller owns the buffer and therefore controls the amount of data
    /// retained at each read. EOF is reported as `Ok(0)`.
    pub async fn read_chunk(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match &mut self.reader {
            OutputReader::Stdout(stdout) => stdout.read(buffer).await,
            OutputReader::Stderr(stderr) => stderr.read(buffer).await,
        }
    }
}

fn stdin_not_piped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "child stdin is not piped")
}

fn stdout_not_piped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "child stdout is not piped")
}

fn stderr_not_piped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "child stderr is not piped")
}

async fn read_owned_to_end<R>(reader: Option<R>) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let Some(mut reader) = reader else {
        return Ok(Vec::new());
    };
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

#[cfg(unix)]
fn signal_process(pid: u32) -> io::Result<()> {
    let result = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(windows)]
fn signal_process(pid: u32) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER};
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            Ok(())
        } else {
            Err(error)
        };
    }
    let terminated = unsafe { TerminateProcess(handle, 1) };
    let termination_error = if terminated == 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };
    unsafe { CloseHandle(handle) };
    termination_error.map_or(Ok(()), Err)
}

/// Build a shell command using the host platform's supported shell.
pub fn shell_spec(command: impl AsRef<OsStr>) -> SpawnSpec {
    #[cfg(windows)]
    {
        SpawnSpec::new("cmd.exe").arg("/C").arg(command.as_ref())
    }
    #[cfg(not(windows))]
    {
        SpawnSpec::new("/bin/sh").arg("-c").arg(command.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::{shell_spec, SpawnSpec, StreamMode};

    fn fixture_command() -> SpawnSpec {
        #[cfg(windows)]
        {
            shell_spec("echo async-platform-internal")
        }
        #[cfg(not(windows))]
        {
            shell_spec("printf async-platform-internal")
        }
    }

    #[tokio::test]
    async fn blessed_spawn_captures_output_without_sync_wait() {
        let output = fixture_command()
            .stdout(StreamMode::Piped)
            .stderr(StreamMode::Piped)
            .spawn()
            .await
            .expect("spawn")
            .wait_with_output()
            .await
            .expect("wait with output");

        assert!(output.status.success());
        let expected = if cfg!(windows) {
            b"async-platform-internal\r\n".as_slice()
        } else {
            b"async-platform-internal".as_slice()
        };
        assert_eq!(output.stdout, expected);
        assert!(output.stderr.is_empty());
    }

    #[tokio::test]
    async fn blessed_spawn_reports_missing_program() {
        let result = SpawnSpec::new("running-process-program-that-does-not-exist")
            .spawn()
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn one_shot_output_closes_owned_stdin() {
        #[cfg(windows)]
        let spec = shell_spec("more > nul & echo done");
        #[cfg(not(windows))]
        let spec = shell_spec("cat > /dev/null; printf done");

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            spec.stdin(StreamMode::Piped)
                .stdout(StreamMode::Piped)
                .stderr(StreamMode::Piped)
                .spawn()
                .await
                .expect("spawn")
                .wait_with_output(),
        )
        .await
        .expect("stdin is closed for one-shot output")
        .expect("output succeeds");

        let expected = if cfg!(windows) {
            b"done\r\n".as_slice()
        } else {
            b"done".as_slice()
        };
        assert_eq!(output.stdout, expected);
    }
}
