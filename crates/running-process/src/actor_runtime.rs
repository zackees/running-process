//! The process-global actor runtime and the sync adapter over it (#850).
//!
//! This module is compiled unconditionally: `NativeProcess` observes its
//! child's lifecycle from a task on this runtime rather than from a dedicated
//! thread per process. The async process actors (`process_runtime`, behind
//! `async-process`) share the same runtime, so there is exactly one engine
//! scheduler in the process.
//!
//! The base runtime has a scheduler and timers only. The I/O driver is
//! enabled only when `async-process` compiles the pipe-owning actors, so a
//! `default-features = false` build does not pull in `mio`.

use std::future::Future;
use std::sync::mpsc;
use std::sync::OnceLock;

use tokio::runtime::{Builder, Handle, Runtime};

static ACTOR_RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Return the library-owned runtime used by process actors.
pub(crate) fn runtime() -> &'static Runtime {
    ACTOR_RUNTIME.get_or_init(|| {
        let mut builder = Builder::new_multi_thread();
        builder
            .worker_threads(runtime_worker_threads())
            .enable_time()
            .thread_name("running-process-actor");
        #[cfg(feature = "async-process")]
        builder.enable_io();
        builder.build().expect("process runtime must initialize")
    })
}

pub(crate) fn runtime_worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(2)
        .clamp(2, 4)
}

/// Drive `future` to completion on the actor runtime from synchronous code.
///
/// Unlike `process_runtime::block_on` (which backs the documented
/// `AsyncProcess::*_blocking` contract and rejects Tokio contexts), this is
/// safe to call from anywhere, including a Tokio worker:
///
/// * outside any runtime, the calling thread drives the future with
///   `Runtime::block_on`;
/// * inside some other runtime, the future is spawned onto the actor runtime
///   and the caller parks on a `std` channel. It never calls `block_on` on a
///   worker, and never uses `oneshot::blocking_recv`, which panics there;
/// * inside the actor runtime itself, the caller additionally enters
///   `block_in_place`, so parking it cannot starve the small worker pool that
///   has to complete the spawned future.
///
/// Parking a foreign runtime's worker is exactly what the previous
/// condvar-based sync API did; this keeps that behaviour rather than turning
/// a sync call inside async code into an error.
pub(crate) fn block_on_anywhere<F>(future: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let actor = runtime();
    let Ok(current) = Handle::try_current() else {
        return actor.block_on(future);
    };
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    actor.spawn(async move {
        let _ = reply_tx.send(future.await);
    });
    let receive = move || {
        reply_rx
            .recv()
            .expect("actor runtime dropped a sync adapter task")
    };
    if current.id() == actor.handle().id() {
        tokio::task::block_in_place(receive)
    } else {
        receive()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{block_on_anywhere, runtime, runtime_worker_threads};

    #[test]
    fn worker_count_is_bounded() {
        assert!((2..=4).contains(&runtime_worker_threads()));
    }

    #[test]
    fn adapter_runs_outside_any_runtime() {
        assert_eq!(block_on_anywhere(async { 7 }), 7);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn adapter_is_safe_on_a_current_thread_runtime() {
        let value = block_on_anywhere(async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            11
        });
        assert_eq!(value, 11);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adapter_is_safe_on_a_multi_thread_runtime() {
        assert_eq!(block_on_anywhere(async { 13 }), 13);
    }

    #[test]
    fn adapter_is_safe_on_the_actor_runtime_itself() {
        let value = runtime()
            .block_on(async { tokio::spawn(async { block_on_anywhere(async { 17 }) }).await })
            .expect("task joins");
        assert_eq!(value, 17);
    }
}
