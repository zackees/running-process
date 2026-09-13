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
