//! Completion, rather than cancellation request, owns the pipe-buffer boundary.

use std::io;
use std::os::windows::io::AsRawHandle;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use windows_sys::Win32::Foundation::ERROR_OPERATION_ABORTED;
use windows_sys::Win32::System::IO::CancelIoEx;

pub(crate) async fn shutdown_output_reader<R>(mut reader: R, pending: bool) -> io::Result<()>
where
    R: AsyncRead + Unpin + AsRawHandle,
{
    if !pending {
        drop(reader);
        return Ok(());
    }

    // The reader remains exclusively owned throughout cancellation and
    // completion. Never target a worker-thread ID: Tokio may reuse that worker
    // for unrelated I/O as soon as this read completes.
    let handle = reader.as_raw_handle() as usize;
    let mut byte = [0];
    let read = reader.read(&mut byte);
    tokio::pin!(read);
    let result = loop {
        // SAFETY: reader owns this live handle until the read future and reader
        // are dropped below. Null OVERLAPPED requests cancellation of this
        // exclusively owned handle's I/O, not I/O on a recycled thread.
        // ERROR_NOT_FOUND can mean the blocking task has not entered ReadFile
        // yet, so retry until the actual read completes; no cancel return value
        // is interpreted as a cleanup acknowledgement.
        unsafe {
            CancelIoEx(handle as _, std::ptr::null());
        }
        tokio::select! {
            result = &mut read => break result,
            _ = tokio::time::sleep(Duration::from_millis(1)) => {}
        }
    };
    // A completed Tokio pipe read has joined its Blocking task and recovered
    // the private buffer. Returning from this scope drops both, before success
    // can be observed. Rust maps ERROR_OPERATION_ABORTED to TimedOut, so match
    // its precise native code rather than swallowing all timeout errors.
    match result {
        Ok(_) => Ok(()),
        Err(error) if error.raw_os_error() == Some(ERROR_OPERATION_ABORTED as i32) => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SpawnSpec, StreamMode};

    struct ReleaseWorker(Option<std::sync::mpsc::Sender<()>>);

    impl Drop for ReleaseWorker {
        fn drop(&mut self) {
            if let Some(release) = self.0.take() {
                let _ = release.send(());
            }
        }
    }

    #[test]
    fn shutdown_retries_when_pipe_read_is_queued_before_its_syscall() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let child = SpawnSpec::new(std::env::current_exe().expect("test executable"))
                .arg("--exact")
                .arg("output_shutdown_tests::silent_output_fixture")
                .env("RUNNING_PROCESS_OUTPUT_SHUTDOWN_FIXTURE", "1")
                .stdin(StreamMode::Null)
                .stdout(StreamMode::Null)
                .stderr(StreamMode::Piped)
                .spawn()
                .await
                .expect("silent child");
            let (mut lifecycle, _, _, _, stderr) = child.into_actor_parts();
            let mut stderr = stderr.expect("silent stderr pipe");
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let release = ReleaseWorker(Some(release_tx));
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = entered_tx.send(());
                let _ = release_rx.recv();
            });
            entered_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("sole blocking worker occupied");
            let mut byte = [0];
            assert!(
                tokio::time::timeout(Duration::from_millis(20), stderr.read_chunk(&mut byte))
                    .await
                    .is_err()
            );
            let result = {
                let shutdown = stderr.shutdown();
                tokio::pin!(shutdown);
                use std::future::Future as _;
                // The sole worker is still occupied: the pipe read cannot yet
                // have entered ReadFile. A first cancellation cannot acknowledge it.
                assert!(shutdown
                    .as_mut()
                    .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                    .is_pending());
                drop(release);
                tokio::time::timeout(Duration::from_secs(2), shutdown).await
            };
            lifecycle.start_kill().expect("kill fixture");
            lifecycle.wait().await.expect("reap fixture");
            blocker.await.expect("join occupied worker");
            result
                .expect("queued read cancellation must retry after worker starts")
                .expect("read cleanup");
        });
    }
}
