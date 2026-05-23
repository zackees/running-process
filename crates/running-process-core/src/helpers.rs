use std::fs::OpenOptions;
use std::io::Write;
use std::time::{Duration, Instant};

pub(crate) const CHILD_PID_LOG_PATH_ENV: &str = "RUNNING_PROCESS_CHILD_PID_LOG_PATH";

/// Hard cap on how long `kill_impl()` will block on
/// `wait_for_capture_completion` after the direct child has been
/// reaped. Override via the `RUNNING_PROCESS_KILL_DRAIN_TIMEOUT_MS`
/// env var (milliseconds). The default of 2 s gives normal children
/// time to flush their pipe buffers while preventing indefinite hangs
/// when a grandchild inherits the pipe and outlives the parent (FastLED
/// Bug B).
pub(crate) const DEFAULT_KILL_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const KILL_DRAIN_TIMEOUT_ENV: &str = "RUNNING_PROCESS_KILL_DRAIN_TIMEOUT_MS";

pub(crate) fn kill_drain_deadline() -> Instant {
    let timeout = std::env::var(KILL_DRAIN_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_KILL_DRAIN_TIMEOUT);
    Instant::now() + timeout
}

pub(crate) fn log_spawned_child_pid(pid: u32) -> Result<(), std::io::Error> {
    let Some(path) = std::env::var_os(CHILD_PID_LOG_PATH_ENV) else {
        return Ok(());
    };

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(format!("{pid}\n").as_bytes())?;
    file.flush()?;
    Ok(())
}

pub(crate) fn feed_chunk(pending: &mut Vec<u8>, chunk: &[u8]) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut index = 0;

    while index < chunk.len() {
        if chunk[index] == b'\n' {
            let end = if index > start && chunk[index - 1] == b'\r' {
                index - 1
            } else {
                index
            };
            pending.extend_from_slice(&chunk[start..end]);
            if !pending.is_empty() {
                lines.push(std::mem::take(pending));
            }
            start = index + 1;
        }
        index += 1;
    }

    pending.extend_from_slice(&chunk[start..]);
    lines
}

pub(crate) fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(1))
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(1)
    }
}
