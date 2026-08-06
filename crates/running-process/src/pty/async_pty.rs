//! Async PTY facade over the bounded synchronous PTY boundary.
//!
//! PTY backends still expose synchronous operations on some supported
//! platforms (notably ConPTY). This module keeps those calls off Tokio
//! workers behind a process-wide, two-permit island. The process actor remains
//! the canonical async engine for pipes; this boundary is intentionally
//! narrow, observable, and temporary until native PTY readiness is available.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::Semaphore;

use super::{NativePtyProcess, PtyError};

const PTY_BLOCKING_CAPACITY: usize = 2;

static PTY_BLOCKING_ISLAND: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn blocking_island() -> Arc<Semaphore> {
    PTY_BLOCKING_ISLAND
        .get_or_init(|| Arc::new(Semaphore::new(PTY_BLOCKING_CAPACITY)))
        .clone()
}

async fn run_blocking<T, F>(operation: F) -> Result<T, PtyError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, PtyError> + Send + 'static,
{
    let permit = blocking_island()
        .acquire_owned()
        .await
        .map_err(|_| PtyError::Other("PTY blocking island is closed".into()))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
    .map_err(|error| PtyError::Other(format!("PTY blocking operation failed: {error}")))?
}

/// Async PTY handle backed by the bounded synchronous PTY island.
///
/// Every operation is dispatched through at most two blocking workers shared
/// by all handles. Dropping an async future stops waiting for its result, but
/// cannot interrupt an OS PTY call already executing; the bounded worker
/// remains responsible for releasing its permit and completing teardown.
pub struct AsyncPtyProcess {
    process: Arc<NativePtyProcess>,
}

impl AsyncPtyProcess {
    /// Construct an async PTY process from the existing PTY configuration.
    pub fn new(
        argv: Vec<String>,
        cwd: Option<String>,
        env: Option<Vec<(String, String)>>,
        rows: u16,
        cols: u16,
        nice: Option<i32>,
    ) -> Result<Self, PtyError> {
        Ok(Self {
            process: Arc::new(NativePtyProcess::new(argv, cwd, env, rows, cols, nice)?),
        })
    }

    /// Start the PTY child without blocking a Tokio worker.
    pub async fn start(&self) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.start_impl()).await
    }

    /// Read one PTY output chunk. `None` means the bounded read timed out.
    pub async fn read_chunk(&self, timeout: Option<Duration>) -> Result<Option<Vec<u8>>, PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.read_chunk_impl(timeout.map(|value| value.as_secs_f64())))
            .await
    }

    /// Write bytes to the PTY input stream.
    pub async fn write(&self, bytes: Vec<u8>, submit: bool) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.write_impl(&bytes, submit)).await
    }

    /// Resize the PTY.
    pub async fn resize(&self, rows: u16, cols: u16) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.resize_impl(rows, cols)).await
    }

    /// Wait for the child and return its exit code.
    pub async fn wait(&self, timeout: Option<Duration>) -> Result<i32, PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.wait_impl(timeout.map(|value| value.as_secs_f64()))).await
    }

    /// Request graceful termination of the PTY child.
    pub async fn terminate(&self) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.terminate_impl()).await
    }

    /// Forcefully terminate the PTY child.
    pub async fn kill(&self) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.kill_impl()).await
    }

    /// Close the PTY and complete bounded teardown.
    pub async fn close(&self) -> Result<(), PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.close_impl()).await
    }

    /// Return the child PID, if the PTY has started.
    pub async fn pid(&self) -> Result<Option<u32>, PtyError> {
        let process = Arc::clone(&self.process);
        run_blocking(move || process.pid()).await
    }
}

#[cfg(test)]
mod tests {
    use super::AsyncPtyProcess;
    use std::time::Duration;

    #[tokio::test]
    async fn async_pty_dispatches_start_read_and_close_through_island() {
        #[cfg(windows)]
        let argv = vec!["cmd.exe".into(), "/C".into(), "echo async-pty".into()];
        #[cfg(not(windows))]
        let argv = vec!["/bin/sh".into(), "-c".into(), "printf async-pty".into()];

        let process =
            AsyncPtyProcess::new(argv, None, None, 24, 80, None).expect("async PTY configuration");
        process.start().await.expect("async PTY start");
        let _ = process.read_chunk(Some(Duration::from_secs(1))).await;
        assert!(process.pid().await.expect("async PTY pid").is_some());
        process.close().await.expect("async PTY close");
    }
}
