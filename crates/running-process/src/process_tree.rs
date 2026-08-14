//! Portable facade for local process-tree termination.
//!
//! Native process-table inspection and platform-specific identity verification
//! are owned by `running-process-platform-internal::platform::process`.

#[cfg(any(test, feature = "async-process"))]
use std::time::Duration;

pub use running_process_platform_internal::platform::process::kill_tree;

/// Await [`kill_tree`] without blocking a Tokio worker.
#[cfg(feature = "async-process")]
pub async fn kill_tree_async(pid: u32, timeout: Duration) -> std::io::Result<u32> {
    crate::blocking_island::dispatch(move || kill_tree(pid, timeout))
        .await
        .map_err(std::io::Error::from)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_pid_is_a_successful_noop() {
        assert_eq!(kill_tree(u32::MAX, Duration::ZERO).unwrap(), 0);
    }

    #[cfg(feature = "async-process")]
    #[tokio::test]
    async fn missing_pid_is_a_successful_noop_on_the_async_form() {
        assert_eq!(kill_tree_async(u32::MAX, Duration::ZERO).await.unwrap(), 0);
    }
}
