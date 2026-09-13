//! Strict snapshot cleanup for a root whose unreaped child is held by the caller.
use super::process_inspect::{process_start_key, StrictProcessHandle};
use std::io;
use std::time::{Duration, Instant};
use sysinfo::{Pid, System};

struct Target {
    pid: u32,
    start: u64,
    handle: StrictProcessHandle,
}

fn changed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "process tree changed during identity capture")
}

fn deadline_check(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        Err(io::Error::new(io::ErrorKind::TimedOut, "owned tree cleanup deadline elapsed"))
    } else { Ok(()) }
}

fn capture(pid: u32) -> io::Result<Target> {
    let start = process_start_key(pid)?;
    let handle = StrictProcessHandle::open(pid)?;
    if handle.has_exited()? || process_start_key(pid)? != start { return Err(changed()); }
    handle.check_signal_permission()?;
    Ok(Target { pid, start, handle })
}

fn parent_pid(pid: u32) -> io::Result<u32> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(format!("/proc/{pid}/stat"))?
        .take(65537).read_to_string(&mut text)?;
    if text.len() > 65536 { return Err(changed()); }
    text.rsplit_once(") ").and_then(|(_, suffix)| suffix.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok()).ok_or_else(changed)
}

fn signal_all<T>(targets: &[T], deadline: Instant, mut signal: impl FnMut(&T) -> io::Result<()>) -> io::Result<()> {
    let mut first = None;
    for target in targets.iter().rev() {
        if let Err(error) = deadline_check(deadline) { return Err(first.unwrap_or(error)); }
        if let Err(error) = signal(target) { first.get_or_insert(error); }
    }
    first.map_or(Ok(()), Err)
}

/// Terminate the captured tree using held pidfds, then confirm terminal state.
/// The root must remain unreaped for this whole call. This is snapshot coverage,
/// not an enclosure: descendants created after enumeration are not guaranteed.
pub(crate) fn kill_tree_owned_root(pid: u32, timeout: Duration) -> io::Result<u32> {
    let deadline = Instant::now().checked_add(timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "tree deadline overflow"))?;
    deadline_check(deadline)?;
    let mut targets = vec![capture(pid)?];
    let mut system = System::new();
    system.refresh_processes();
    // Breadth-first capture ensures a parent handle exists before each child.
    // Reverse traversal below signals children before their parents.
    let mut index = 0;
    while index < targets.len() {
        deadline_check(deadline)?;
        let parent = &targets[index];
        if parent.handle.has_exited()? || process_start_key(parent.pid)? != parent.start {
            return Err(changed());
        }
        let parent_id = parent.pid;
        let parent_start = parent.start;
        let mut children = Vec::new();
        for (child_pid, process) in system.processes() {
            deadline_check(deadline)?;
            if process.parent() != Some(Pid::from_u32(parent_id)) { continue; }
            let child_pid = child_pid.as_u32();
            if targets.iter().any(|target| target.pid == child_pid) { return Err(changed()); }
            let child = capture(child_pid)?;
            if child.start < parent_start || parent_pid(child_pid)? != parent_id {
                return Err(changed());
            }
            children.push(child);
        }
        if targets[index].handle.has_exited()? || process_start_key(parent_id)? != parent_start {
            return Err(changed());
        }
        targets.extend(children);
        index += 1;
    }
    // All handles and signal permissions are acquired before any termination.
    // No subsequent operation reopens a numeric PID to send a signal.
    // A denied or failed signal must not prevent attempts on other already
    // captured process objects. Keep the first error, and never classify this
    // sweep as confirmed cleanup when any termination attempt failed.
    signal_all(&targets, deadline, |target| target.handle.kill())?;
    loop {
        let mut all_exited = true;
        for target in &targets {
            all_exited &= target.handle.has_exited()?;
        }
        if all_exited {
            return u32::try_from(targets.len()).map_err(io::Error::other);
        }
        deadline_check(deadline)?;
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(10)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_budget_never_reaches_process_capture_or_signaling() {
        // An invalid PID would produce a capture error if the budget gate
        // were skipped. No real process is targeted by this regression.
        let error = kill_tree_owned_root(u32::MAX, Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn missing_root_is_not_reported_as_confirmed_tree_cleanup() {
        assert!(kill_tree_owned_root(u32::MAX, Duration::from_secs(1)).is_err());
    }

    #[test]
    fn signal_failure_does_not_skip_remaining_held_targets() {
        let mut seen = Vec::new();
        let error = signal_all(&[1, 2, 3], Instant::now() + Duration::from_secs(1), |value| {
            seen.push(*value);
            match *value {
                2 => Err(io::Error::from_raw_os_error(libc::EPERM)),
                1 => Err(io::Error::from_raw_os_error(libc::EIO)),
                _ => Ok(()),
            }
        }).unwrap_err();
        assert_eq!(seen, vec![3, 2, 1]);
        assert_eq!(error.raw_os_error(), Some(libc::EPERM));
    }

    #[test]
    fn expired_signal_budget_does_not_attempt_any_held_target() {
        let error = signal_all(&[1, 2, 3], Instant::now(), |_| {
            panic!("expired cleanup must not signal a target")
        }).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
