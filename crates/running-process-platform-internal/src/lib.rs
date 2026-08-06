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

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

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
}

impl PlatformChild {
    fn new(child: Child) -> Self {
        Self { child }
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
        self.child.wait_with_output().await
    }

    /// Write bytes to piped stdin and flush them.
    pub async fn write_stdin(&mut self, bytes: &[u8]) -> io::Result<()> {
        let stdin =
            self.child.stdin.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "child stdin is not piped")
            })?;
        stdin.write_all(bytes).await?;
        stdin.flush().await
    }

    /// Close the piped stdin handle, delivering EOF to the child.
    ///
    /// This operation is idempotent. Closing an inherited or null stdin is
    /// also a no-op because there is no owned pipe to close.
    pub fn close_stdin(&mut self) {
        drop(self.child.stdin.take());
    }

    /// Read all bytes from piped stdout without waiting for process exit.
    pub async fn read_stdout_to_end(&mut self) -> io::Result<Vec<u8>> {
        let stdout = self.child.stdout.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "child stdout is not piped")
        })?;
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await?;
        Ok(bytes)
    }

    /// Read all bytes from piped stderr without waiting for process exit.
    pub async fn read_stderr_to_end(&mut self) -> io::Result<Vec<u8>> {
        let stderr = self.child.stderr.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "child stderr is not piped")
        })?;
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await?;
        Ok(bytes)
    }
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
}
