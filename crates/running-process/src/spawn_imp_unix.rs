use std::io;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

pub struct SpawnedInner {
    child: Arc<Mutex<Option<std::process::Child>>>,
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

pub fn spawn_daemon(command: &mut Command, _clear_env: bool) -> io::Result<super::DaemonChild> {
    use std::os::unix::process::CommandExt;

    // `_clear_env` is intentionally ignored on Unix. The reason: on
    // Unix we hand the Command to `command.spawn()` which natively
    // honours `Command::env_clear()` — so the caller is expected to
    // have called `env_clear()` BEFORE adding their env overrides via
    // `command.envs(...)`. Calling `env_clear()` HERE would wipe the
    // overrides too (`CommandEnv::clear()` resets the vars vec along
    // with the clear flag), which silently broke the daemon's
    // env-replace path on Linux until this was found.
    //
    // On Windows the equivalent signal is needed because our manual
    // `build_env_block` doesn't see Rust stdlib's clear flag through
    // `Command::get_envs()`; that path consumes the bool explicitly.

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
) -> io::Result<super::SpawnedChild> {
    use std::os::unix::process::CommandExt;

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

    let child = Arc::new(Mutex::new(Some(child)));

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
