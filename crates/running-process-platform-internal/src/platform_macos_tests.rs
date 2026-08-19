use std::ffi::OsStr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;

#[tokio::test]
async fn owner_death_registration_failure_aborts_spawn() {
    let mut command = tokio::process::Command::new("/usr/bin/true");
    super::configure_command_for_owner(&mut command, false, true, libc::pid_t::MAX)
        .expect("configure owner-death containment");

    match command.spawn() {
        Ok(mut child) => {
            let _ = child.kill().await;
            panic!("spawn succeeded before the owner watch was registered");
        }
        Err(error) => assert_eq!(
            error.raw_os_error(),
            Some(libc::ESRCH),
            "spawn must report the failed owner-watch registration"
        ),
    }
}

#[tokio::test]
async fn owner_death_supervisor_does_not_retain_child_stdout() {
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .args(["-c", "exec 1>&-; exec sleep 30"])
        .stdout(Stdio::piped());
    super::configure_command_for_owner(
        &mut command,
        false,
        true,
        unsafe { libc::getpid() },
    )
    .expect("configure owner-death containment");

    let mut child = command.spawn().expect("spawn stream-closing child");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut output = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), stdout.read_to_end(&mut output))
        .await
        .expect("supervisor retained the child's closed stdout")
        .expect("read child stdout");
    assert!(output.is_empty());
    child.kill().await.expect("kill stream-closing child");
}

#[tokio::test]
async fn owner_death_supervisor_closes_multiple_fd_batches() {
    const EXTRA_FD_COUNT: usize = 96;

    let extra_fds = (0..EXTRA_FD_COUNT)
        .map(|_| {
            let fd = unsafe {
                libc::open(
                    c"/dev/null".as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC,
                )
            };
            assert!(fd >= 0, "open inheritable test descriptor: {}", std::io::Error::last_os_error());
            // SAFETY: `open` returned a new descriptor owned by this test.
            unsafe { OwnedFd::from_raw_fd(fd) }
        })
        .collect::<Vec<_>>();

    let mut probe = [-1; 2];
    assert_eq!(unsafe { libc::pipe(probe.as_mut_ptr()) }, 0, "create EOF probe pipe");
    // SAFETY: `pipe` returned two new descriptors owned by this test.
    let probe_reader = unsafe { OwnedFd::from_raw_fd(probe[0]) };
    // SAFETY: `pipe` returned two new descriptors owned by this test.
    let probe_writer = unsafe { OwnedFd::from_raw_fd(probe[1]) };
    for fd in [&probe_reader, &probe_writer] {
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0, "read EOF probe descriptor flags");
        assert_eq!(
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) },
            0,
            "mark EOF probe descriptor close-on-exec"
        );
    }

    let mut command = tokio::process::Command::new("/bin/sleep");
    command.arg("30");
    super::configure_command_for_owner(
        &mut command,
        false,
        true,
        unsafe { libc::getpid() },
    )
    .expect("configure owner-death containment");

    let mut child = command
        .spawn()
        .expect("spawn child with multiple descriptor batches");
    drop(probe_writer);
    drop(extra_fds);

    let eof_result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let mut descriptor = libc::pollfd {
            fd: probe_reader.as_raw_fd(),
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        loop {
            let ready = unsafe { libc::poll(&mut descriptor, 1, 2_000) };
            if ready == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "supervisor retained a descriptor beyond its first batch",
                ));
            }
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            let mut byte = 0_u8;
            let bytes = unsafe {
                libc::read(
                    probe_reader.as_raw_fd(),
                    (&mut byte as *mut u8).cast(),
                    1,
                )
            };
            return if bytes == 0 {
                Ok(())
            } else if bytes < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "unexpected data in supervisor EOF probe",
                ))
            };
        }
    })
        .await
        .expect("join supervisor EOF probe");
    child.kill().await.expect("kill descriptor-batch child");
    eof_result.expect("wait for supervisor to close the high-numbered descriptor");
}

#[test]
fn shell_command_preserves_login_shell_contract_and_ignores_child_path() {
    let command_text = "printf '%s' 'alpha beta;\"gamma\"'";
    let mut command = super::shell_command(command_text);
    assert_eq!(command.get_program(), OsStr::new("/bin/sh"));
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        [OsStr::new("-lc"), OsStr::new(command_text)]
    );
    command
        .env_clear()
        .env("PATH", "/caller-supplied-path-override");
    let output = command
        .output()
        .expect("absolute shell command should execute independently of child PATH");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"alpha beta;\"gamma\"");
}
