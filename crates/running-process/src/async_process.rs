//! Native asynchronous pipe-process API.
//!
//! The platform crate owns Tokio's process primitives. This module exposes a
//! stable process-facing API without re-exporting `tokio::process` types.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{ExitStatus, Output};

use running_process_platform_internal::{SpawnSpec, StreamMode};

use crate::process_runtime::ActorProcess;
use crate::{ProcessError, RunOutput};

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

    /// Start the configured process.
    pub async fn start(&mut self) -> Result<(), ProcessError> {
        if self.child.is_some() {
            return Err(ProcessError::AlreadyStarted);
        }
        self.child = Some(ActorProcess::start(self.spec.clone()).await?);
        Ok(())
    }

    /// Return the child process identifier after [`Self::start`].
    pub async fn pid(&self) -> Result<u32, ProcessError> {
        self.child
            .as_ref()
            .ok_or(ProcessError::NotRunning)?
            .pid()
            .await
    }

    /// Wait for the started process without capturing output.
    pub async fn wait(&mut self) -> Result<ExitStatus, ProcessError> {
        self.child
            .as_mut()
            .ok_or(ProcessError::NotRunning)?
            .wait()
            .await
    }

    /// Kill the started process.
    pub async fn kill(&mut self) -> Result<(), ProcessError> {
        self.child
            .as_mut()
            .ok_or(ProcessError::NotRunning)?
            .kill()
            .await
    }

    /// Wait for completion and return captured stdout/stderr.
    pub async fn output(&mut self) -> Result<RunOutput, ProcessError> {
        let child = self.child.as_ref().ok_or(ProcessError::NotRunning)?;
        let output = child.output().await?;
        Ok(run_output(output))
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

    async fn output_after_start(mut self) -> Result<RunOutput, ProcessError> {
        self.start().await?;
        self.output().await
    }
}

fn run_output(output: Output) -> RunOutput {
    RunOutput {
        stdout: output.stdout,
        stderr: output.stderr,
        exit_code: output.status.code().unwrap_or_else(|| {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                return -output.status.signal().unwrap_or(1);
            }
            #[cfg(not(unix))]
            {
                -1
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::AsyncProcess;

    #[tokio::test]
    async fn async_process_captures_stdout_and_stderr() {
        #[cfg(windows)]
        let process = AsyncProcess::new("cmd.exe")
            .arg("/C")
            .arg("echo out && echo err 1>&2");
        #[cfg(not(windows))]
        let process = AsyncProcess::new("/bin/sh")
            .arg("-c")
            .arg("printf out; printf err >&2");

        let output = process.output_after_start().await.expect("async output");
        let expected_stdout = if cfg!(windows) {
            b"out \r\n".as_slice()
        } else {
            b"out".as_slice()
        };
        let expected_stderr = if cfg!(windows) {
            b"err \r\n".as_slice()
        } else {
            b"err".as_slice()
        };
        assert_eq!(output.stdout, expected_stdout);
        assert_eq!(output.stderr, expected_stderr);
        assert_eq!(output.exit_code, 0);
    }

    #[tokio::test]
    async fn async_process_rejects_double_start() {
        #[cfg(windows)]
        let mut process = AsyncProcess::new("cmd.exe").arg("/C").arg("exit 0");
        #[cfg(not(windows))]
        let mut process = AsyncProcess::new("/bin/true");
        process.start().await.expect("first start");
        assert!(matches!(
            process.start().await,
            Err(crate::ProcessError::AlreadyStarted)
        ));
        process.kill().await.ok();
    }
}
