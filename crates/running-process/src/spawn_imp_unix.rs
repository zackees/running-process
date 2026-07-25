use std::io;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

trait UnixChild: Send {
    fn kill(&mut self) -> io::Result<()>;
    fn wait(&mut self) -> io::Result<std::process::ExitStatus>;
    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>>;
}

impl UnixChild for std::process::Child {
    fn kill(&mut self) -> io::Result<()> {
        std::process::Child::kill(self)
    }

    fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        std::process::Child::wait(self)
    }

    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        std::process::Child::try_wait(self)
    }
}

pub struct SpawnedInner {
    child: Arc<Mutex<Option<Box<dyn UnixChild>>>>,
    pgid: i32,
}

impl SpawnedInner {
    pub fn kill(&self) -> io::Result<()> {
        // Try the child first, then the process group, to make sure
        // any siblings spawned inside go down too.
        let mut guard = self.child.lock().expect("child mutex poisoned");
        if let Some(child) = guard.as_mut() {
            let _ = child.kill();
        }
        drop(guard);
        unsafe {
            libc::killpg(self.pgid, libc::SIGKILL);
        }
        Ok(())
    }

    pub fn wait(&self) -> io::Result<i32> {
        let mut guard = self.child.lock().expect("child mutex poisoned");
        let Some(child) = guard.as_mut() else {
            return Err(io::Error::other("child handle absent"));
        };
        let status = child.wait()?;
        Ok(super::unix_exit_code(status))
    }

    pub fn try_wait(&self) -> io::Result<Option<i32>> {
        let mut guard = self.child.lock().expect("child mutex poisoned");
        let Some(child) = guard.as_mut() else {
            return Ok(None);
        };
        Ok(child.try_wait()?.map(super::unix_exit_code))
    }

    pub fn shutdown(&mut self) {
        unsafe {
            libc::killpg(self.pgid, libc::SIGKILL);
        }
        // Reap.
        let mut guard = self.child.lock().expect("child mutex poisoned");
        if let Some(child) = guard.as_mut() {
            let _ = child.wait();
        }
    }
}

fn slot_to_stdio(slot: &super::StdioSource<'_>) -> io::Result<Stdio> {
    match slot {
        super::StdioSource::Null => Ok(Stdio::null()),
        super::StdioSource::Parent => Ok(Stdio::inherit()),
        super::StdioSource::Fd(fd) => {
            let owned = fd.try_clone_to_owned()?;
            Ok(Stdio::from(owned))
        }
        super::StdioSource::Pipe => Ok(Stdio::piped()),
        super::StdioSource::_Phantom(_) => unreachable!(),
    }
}

pub fn spawn_daemon(
    command: &mut Command,
    policy: super::EnvironmentPolicy,
) -> io::Result<super::DaemonChild> {
    use std::os::unix::process::CommandExt;

    apply_environment_policy(command, policy);

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                // Already a session leader — not fatal.
            }
            close_extra_fds();
            Ok(())
        });
    }

    let child = command.spawn()?;
    let pid = child.id();
    Ok(super::DaemonChild { pid, child })
}

pub fn spawn(
    command: &mut Command,
    stdio: super::SpawnStdio<'_>,
    policy: super::EnvironmentPolicy,
) -> io::Result<super::SpawnedChild> {
    use std::os::unix::process::CommandExt;

    apply_environment_policy(command, policy);
    command.stdin(slot_to_stdio(&stdio.stdin)?);
    command.stdout(slot_to_stdio(&stdio.stdout)?);
    command.stderr(slot_to_stdio(&stdio.stderr)?);

    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() == 1 {
                    libc::_exit(1);
                }
            }
            close_extra_fds();
            Ok(())
        });
    }

    let mut child = command.spawn()?;
    let pid = child.id();
    let pgid = pid as i32;

    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let child: Arc<Mutex<Option<Box<dyn UnixChild>>>> = Arc::new(Mutex::new(Some(Box::new(child))));

    // Drain watcher: wait for exit, then sleep `drain_timeout`. We
    // don't proactively close anything on Unix — Rust's ChildStdin/etc.
    // own their fds; once the child exits and the kernel ref-counts
    // its copies to zero, parent reads will EOF naturally.
    if let Some(timeout) = stdio.drain_timeout {
        let child_clone = Arc::clone(&child);
        thread::spawn(move || {
            // Borrow child for try_wait.  We do a polling loop so
            // shutdown() taking the inner Child during Drop doesn't
            // wedge us.
            loop {
                {
                    let mut guard = child_clone.lock().expect("child mutex poisoned");
                    match guard.as_mut() {
                        Some(c) => match c.try_wait() {
                            Ok(Some(_)) => break,
                            Ok(None) => {}
                            Err(_) => break,
                        },
                        None => return,
                    }
                }
                // #199: intentional — try_wait poll on the contained
                // child, 50ms cadence inside a bounded outer drain
                // loop. waitpid(WNOHANG)-equivalent semantics.
                thread::sleep(std::time::Duration::from_millis(50));
            }
            // #199: intentional — post-mortem pipe drain. Children's
            // write-ends of the captured stdio pipes are still being
            // closed by the kernel after exit; this gives readers a
            // chance to see the final bytes before the watcher
            // releases its keep-alive.
            thread::sleep(timeout);
        });
    }

    Ok(super::SpawnedChild {
        stdin,
        stdout,
        stderr,
        pid,
        inner: SpawnedInner { child, pgid },
    })
}

fn apply_environment_policy(command: &mut Command, policy: super::EnvironmentPolicy) {
    match policy {
        super::EnvironmentPolicy::Clear => {
            // Preserve explicit Command::env overrides while changing the
            // base to empty. Command::env_clear() also clears the override
            // map, so snapshot and restore it.
            let explicit: Vec<_> = command
                .get_envs()
                .filter_map(|(key, value)| {
                    value.map(|value| (key.to_os_string(), value.to_os_string()))
                })
                .collect();
            command.env_clear();
            command.envs(explicit);
        }
        super::EnvironmentPolicy::Inherit
        | super::EnvironmentPolicy::UserBaseline
        | super::EnvironmentPolicy::Auto => {
            // There is no stable Unix API equivalent to Windows
            // CreateEnvironmentBlock. UserBaseline conservatively falls
            // back to inheritance on Unix.
        }
    }
}

/// Async-signal-safe fd sweep used in pre_exec. See sanitized.rs (now
/// merged here) for the rationale.
unsafe fn close_extra_fds() {
    #[cfg(target_os = "linux")]
    {
        #[cfg(any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "x86",
            target_arch = "arm",
            target_arch = "riscv64",
            target_arch = "powerpc64",
        ))]
        {
            const SYS_CLOSE_RANGE: libc::c_long = 436;
            let rc = libc::syscall(SYS_CLOSE_RANGE, 3u32, libc::c_uint::MAX, 0u32);
            if rc == 0 {
                return;
            }
        }
    }

    let dir = libc::opendir(c"/dev/fd".as_ptr());
    if !dir.is_null() {
        let dir_fd = libc::dirfd(dir);
        loop {
            let ent = libc::readdir(dir);
            if ent.is_null() {
                break;
            }
            let name_ptr = (*ent).d_name.as_ptr();
            let mut fd: libc::c_int = 0;
            let mut p = name_ptr;
            let mut ok = false;
            while *p != 0 {
                let c = *p as u8;
                if !c.is_ascii_digit() {
                    ok = false;
                    break;
                }
                fd = fd * 10 + (c - b'0') as libc::c_int;
                p = p.add(1);
                ok = true;
            }
            if !ok {
                continue;
            }
            if fd > 2 && fd != dir_fd {
                libc::close(fd);
            }
        }
        libc::closedir(dir);
        return;
    }

    let max = libc::sysconf(libc::_SC_OPEN_MAX);
    let max = if max < 0 { 4096 } else { max as libc::c_int };
    for fd in 3..max {
        libc::close(fd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, mpsc};
    use std::time::{Duration, Instant};

    struct FakeChild {
        wait_gate: Arc<(Mutex<bool>, Condvar)>,
        waits: Arc<AtomicUsize>,
    }

    impl UnixChild for FakeChild {
        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
            self.waits.fetch_add(1, Ordering::SeqCst);
            let (lock, condvar) = &*self.wait_gate;
            let mut released = lock.lock().expect("wait gate mutex poisoned");
            while !*released {
                released = condvar.wait(released).expect("wait gate mutex poisoned");
            }
            Ok(std::process::ExitStatus::from_raw(0))
        }

        fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
            Ok(None)
        }
    }

    struct BlockedFixture {
        inner: SpawnedInner,
        child: Arc<Mutex<Option<Box<dyn UnixChild>>>>,
        wait_gate: Arc<(Mutex<bool>, Condvar)>,
        waits: Arc<AtomicUsize>,
    }

    fn blocked_inner() -> BlockedFixture {
        let wait_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let waits = Arc::new(AtomicUsize::new(0));
        let child: Arc<Mutex<Option<Box<dyn UnixChild>>>> =
            Arc::new(Mutex::new(Some(Box::new(FakeChild {
                wait_gate: Arc::clone(&wait_gate),
                waits: Arc::clone(&waits),
            }))));
        BlockedFixture {
            inner: SpawnedInner {
                child: Arc::clone(&child),
                pgid: i32::MAX,
            },
            child,
            wait_gate,
            waits,
        }
    }

    fn release_wait(wait_gate: &Arc<(Mutex<bool>, Condvar)>) {
        let (lock, condvar) = &**wait_gate;
        *lock.lock().expect("wait gate mutex poisoned") = true;
        condvar.notify_all();
    }

    struct ShutdownOnDrop(Option<SpawnedInner>);

    impl Drop for ShutdownOnDrop {
        fn drop(&mut self) {
            self.0
                .as_mut()
                .expect("test wrapper missing inner")
                .shutdown();
        }
    }

    #[test]
    fn drop_is_bounded_when_child_wait_does_not_complete() {
        // Regression for #619: SpawnedChild::drop delegates directly to
        // SpawnedInner::shutdown, modeled by this wrapper around the fake child.
        let BlockedFixture {
            inner, wait_gate, ..
        } = blocked_inner();
        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        let worker = thread::spawn(move || {
            drop(ShutdownOnDrop(Some(inner)));
            let _ = tx.send(started.elapsed());
        });

        let timely = rx.recv_timeout(Duration::from_millis(100));
        release_wait(&wait_gate);
        let elapsed = timely
            .or_else(|_| rx.recv_timeout(Duration::from_secs(1)))
            .expect("shutdown did not unblock after releasing fake child");
        worker.join().expect("shutdown worker panicked");
        assert!(
            elapsed < Duration::from_millis(100),
            "Drop blocked for {elapsed:?} in child.wait()"
        );
    }

    #[test]
    fn shutdown_does_not_hold_child_mutex_while_reaping() {
        let BlockedFixture {
            mut inner,
            child,
            wait_gate,
            waits,
        } = blocked_inner();
        let worker = thread::spawn(move || inner.shutdown());
        let deadline = Instant::now() + Duration::from_secs(1);
        while waits.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            thread::yield_now();
        }
        assert_eq!(waits.load(Ordering::SeqCst), 1, "fake wait never started");

        let child_mutex_available = child.try_lock().is_ok();
        release_wait(&wait_gate);
        worker.join().expect("shutdown worker panicked");
        assert!(
            child_mutex_available,
            "shutdown held the child mutex across reaping"
        );
    }
}
