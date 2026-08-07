//! The one bounded island for OS operations that have no asynchronous form.
//!
//! Some operations simply do not exist as futures on any supported platform:
//! ConPTY is synchronous, and enumerating a process tree means a blocking
//! snapshot of the OS process table. The rule for those is not "never block" --
//! it is "block in exactly one place, with a fixed ceiling, where the cost is
//! visible".
//!
//! Every such call goes through [`dispatch`], which holds one of
//! [`ISLAND_CAPACITY`] permits for the duration of the blocking call. The
//! ceiling is process-wide and shared by every handle, so no number of
//! concurrent callers can grow the blocking footprint. That is what makes this
//! a bounded island rather than an unbounded escape hatch: `spawn_blocking`
//! alone would let Tokio grow its blocking pool to hundreds of threads under
//! load.
//!
//! Cancelling a future returned by [`dispatch`] stops the caller waiting for
//! the result. It cannot interrupt an OS call already executing -- the worker
//! stays responsible for finishing it and releasing its permit.

use std::sync::{Arc, OnceLock};

use tokio::sync::Semaphore;

/// Concurrent blocking operations allowed across the whole process.
pub(crate) const ISLAND_CAPACITY: usize = 2;

static ISLAND: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn island() -> Arc<Semaphore> {
    ISLAND
        .get_or_init(|| Arc::new(Semaphore::new(ISLAND_CAPACITY)))
        .clone()
}

/// Why a dispatched operation never produced a result.
///
/// This is never the operation's own error -- it is the island failing to run
/// it at all, which callers map onto their own error type.
#[derive(Debug)]
pub(crate) struct IslandFailure(String);

impl std::fmt::Display for IslandFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<IslandFailure> for std::io::Error {
    fn from(failure: IslandFailure) -> Self {
        std::io::Error::other(failure.0)
    }
}

/// Run one blocking operation on the bounded island.
pub(crate) async fn dispatch<T, F>(operation: F) -> Result<T, IslandFailure>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let permit = island()
        .acquire_owned()
        .await
        .map_err(|_| IslandFailure("blocking island is closed".into()))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
    .map_err(|error| IslandFailure(format!("blocking operation failed: {error}")))
}

/// Run a blocking operation on the shared island, for callers outside this crate.
///
/// Exposed for the PyO3 layer, which needs the async counterparts of the
/// process-tree helpers to land on the *same* ceiling as everything else. A
/// second island in the bindings would defeat the point of having one.
///
/// The operation must not itself block on a future; it runs on a blocking
/// worker where awaiting is not possible.
pub async fn dispatch_blocking<T, F>(operation: F) -> std::io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    dispatch(operation).await.map_err(std::io::Error::from)
}

#[cfg(test)]
mod tests {
    use super::{dispatch, island, ISLAND_CAPACITY};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_dispatch_never_exceeds_the_island_capacity() {
        // The point of the island is the ceiling, so assert the ceiling
        // rather than merely that the operations ran.
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..(ISLAND_CAPACITY * 8) {
            let live = Arc::clone(&live);
            let peak = Arc::clone(&peak);
            handles.push(tokio::spawn(async move {
                dispatch(move || {
                    let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    live.fetch_sub(1, Ordering::SeqCst);
                })
                .await
                .expect("island dispatch")
            }));
        }
        for handle in handles {
            handle.await.expect("dispatch task");
        }
        assert!(
            peak.load(Ordering::SeqCst) <= ISLAND_CAPACITY,
            "island allowed {} concurrent blocking calls, ceiling is {ISLAND_CAPACITY}",
            peak.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn dispatch_returns_the_operation_result() {
        assert_eq!(dispatch(|| 7_u32).await.expect("island dispatch"), 7);
    }

    #[tokio::test]
    async fn every_permit_is_released_after_the_operation_completes() {
        // A leaked permit would not fail any single call -- it would quietly
        // shrink the island until it deadlocked, so check the count directly.
        for _ in 0..(ISLAND_CAPACITY * 4) {
            dispatch(|| ()).await.expect("island dispatch");
        }
        assert_eq!(island().available_permits(), ISLAND_CAPACITY);
    }
}
