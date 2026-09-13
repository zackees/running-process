//! Owned-handle snapshot termination; never reopen a PID to signal it.
use super::process_inspect::ProcessLiveness;
use std::io;
use std::time::{Duration, Instant};
use sysinfo::{Pid, System};

struct Target {
    pid: u32,
    created: u64,
    handle: ProcessLiveness,
}

fn changed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "process tree changed during handle capture")
}

fn check_deadline(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        Err(io::Error::new(io::ErrorKind::TimedOut, "owned tree cleanup deadline elapsed"))
    } else { Ok(()) }
}

fn capture(pid: u32) -> io::Result<Target> {
    let handle = ProcessLiveness::open_for_control(pid).map_err(|error| error.source)?;
    let created = handle.creation_time()?;
    if handle.has_exited()? { return Err(changed()); }
    Ok(Target { pid, created, handle })
}

fn signal_all<T>(targets: &[T], deadline: Instant, mut signal: impl FnMut(&T) -> io::Result<()>) -> io::Result<()> {
    let mut first = None;
    for target in targets.iter().rev() {
        if let Err(error) = check_deadline(deadline) { return Err(first.unwrap_or(error)); }
        if let Err(error) = signal(target) { first.get_or_insert(error); }
    }
    first.map_or(Ok(()), Err)
}

/// Caller retains its original child handle for the entire operation. Captured
/// descendants also remain held until exit confirmation, preventing PID reuse
/// from redirecting control. Coverage is a snapshot, not a Job enclosure.
pub(crate) fn kill_tree_owned_root(pid: u32, timeout: Duration) -> io::Result<u32> {
    let deadline = Instant::now().checked_add(timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "tree deadline overflow"))?;
    check_deadline(deadline)?;
    let mut targets = vec![capture(pid)?];
    let mut snapshot = System::new();
    snapshot.refresh_processes();
    let mut index = 0;
    while index < targets.len() {
        check_deadline(deadline)?;
        if targets[index].handle.has_exited()? { return Err(changed()); }
        let parent_pid = targets[index].pid;
        let parent_created = targets[index].created;
        let mut children = Vec::new();
        for (candidate, process) in snapshot.processes() {
            if process.parent() != Some(Pid::from_u32(parent_pid)) { continue; }
            check_deadline(deadline)?;
            let child_pid = candidate.as_u32();
            if targets.iter().any(|target| target.pid == child_pid) { return Err(changed()); }
            let child = capture(child_pid)?;
            if child.created < parent_created { return Err(changed()); }
            // The initial snapshot may predate handle acquisition. Refresh
            // parentage now that both process objects are held and their PIDs
            // cannot be reused, rather than trusting stale snapshot parentage.
            let mut current = System::new();
            current.refresh_processes();
            if current.process(*candidate).and_then(|process| process.parent())
                != Some(Pid::from_u32(parent_pid))
                || child.handle.has_exited()?
            {
                return Err(changed());
            }
            children.push(child);
        }
        if targets[index].handle.has_exited()? { return Err(changed()); }
        targets.extend(children);
        index += 1;
    }
    // A denied or failed signal must not prevent attempts on other already
    // captured process objects. Keep the first error, and never classify this
    // sweep as confirmed cleanup when any termination attempt failed.
    signal_all(&targets, deadline, |target| target.handle.kill())?;
    loop {
        let mut all_exited = true;
        for target in &targets { all_exited &= target.handle.has_exited()?; }
        if all_exited { return u32::try_from(targets.len()).map_err(io::Error::other); }
        check_deadline(deadline)?;
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(10)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_deadline_precedes_handle_acquisition() {
        let error = kill_tree_owned_root(u32::MAX, Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn absent_root_is_not_confirmed_tree_cleanup() {
        assert!(kill_tree_owned_root(u32::MAX, Duration::from_secs(1)).is_err());
    }

    #[test]
    fn signal_failure_still_attempts_all_held_targets_and_keeps_first_error() {
        let mut seen = Vec::new();
        let error = signal_all(&[1, 2, 3], Instant::now() + Duration::from_secs(1), |value| {
            seen.push(*value);
            match *value { 3 => Err(io::Error::from_raw_os_error(5)), 2 => Err(io::Error::from_raw_os_error(6)), _ => Ok(()) }
        }).unwrap_err();
        assert_eq!(seen, vec![3, 2, 1]);
        assert_eq!(error.raw_os_error(), Some(5));
    }

    #[test]
    fn expired_signal_budget_does_not_attempt_any_held_target() {
        let error = signal_all(&[1, 2, 3], Instant::now(), |_| {
            panic!("expired cleanup must not signal a target")
        }).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
