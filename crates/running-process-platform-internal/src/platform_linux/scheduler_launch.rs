//! Immediate user-service launching. Only the helper and private request-file
//! path enter manager metadata; application argv/environment never do.

use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::platform::process::{SpawnStdio, StdioSource, SyncEnvironment};

const OUTPUT_LIMIT: usize = 8192;

pub(super) struct ScheduledUnit {
    name: String,
    cleanup_on_drop: bool,
}

impl ScheduledUnit {
    pub(super) fn verify_helper_placement(
        &self,
        worker: &super::resource_placement::Placement,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<super::process_inspect::ProcessLiveness> {
        let process = super::process_inspect::ProcessLiveness::open_pinned(
            self.main_pid(deadline, cancelled)?,
        )?;
        let placement = super::resource_placement::Placement::capture_pinned(&process)?;
        if !placement.outside_worker(worker)? {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "service manager retained caller containment",
            ));
        }
        Ok(process)
    }

    /// Starting the helper is not target readiness. Retain this rollback guard
    /// until the helper handshake proves the actual target's identity/placement.
    pub(super) fn launch(
        helper: &Path,
        request: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Self> {
        check_budget(deadline, cancelled)?;
        let mut entropy = [0_u8; 16];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut entropy)?;
        let suffix: String = entropy.iter().map(|byte| format!("{byte:02x}")).collect();
        let name = format!("rp-independent-{suffix}.service");
        let mut command = launch_command(&name, helper, request)?;
        let unit = Self {
            name,
            cleanup_on_drop: true,
        };
        run(&mut command, deadline, cancelled)?;
        Ok(unit)
    }

    pub(super) fn main_pid(&self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<u32> {
        let mut command = Command::new("systemctl");
        command.args([
            "--user",
            "--no-pager",
            "show",
            "--property=MainPID",
            "--value",
            &self.name,
        ]);
        let bytes = run(&mut command, deadline, cancelled)?;
        let pid = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok());
        match pid {
            Some(pid) if pid > 0 && pid <= i32::MAX as u32 => Ok(pid),
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "scheduled helper is not running",
            )),
        }
    }

    pub(super) fn stop(&mut self, deadline: Instant) -> io::Result<()> {
        let mut command = Command::new("systemctl");
        command.args(["--user", "--no-pager", "stop", &self.name]);
        if let Err(error) = run(&mut command, deadline, &AtomicBool::new(false)) {
            if error.kind() != io::ErrorKind::NotFound {
                return Err(error);
            }
        }
        self.cleanup_on_drop = false;
        Ok(())
    }

    /// Commit detached lifetime only after the target handshake succeeds.
    pub(super) fn retain_on_drop(&mut self) {
        self.cleanup_on_drop = false;
    }
}

impl Drop for ScheduledUnit {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let _ = self.stop(Instant::now() + Duration::from_secs(2));
        }
    }
}

fn launch_command(name: &str, helper: &Path, request: &Path) -> io::Result<Command> {
    if !helper.is_absolute() || !request.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "scheduler paths must be absolute",
        ));
    }
    let mut command = Command::new("systemd-run");
    command.args([
        "--user",
        "--quiet",
        "--collect",
        "--service-type=exec",
        "--expand-environment=no",
        "--description=running-process independent launcher",
        "--property=StandardInput=null",
        "--property=StandardOutput=null",
        "--property=StandardError=null",
        "--unit",
        name,
        "--",
    ]);
    command.arg(helper).arg(request);
    Ok(command)
}

fn nonblocking(fd: &impl AsRawFd) -> io::Result<()> {
    // SAFETY: fcntl acts on a borrowed live descriptor, without taking ownership.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain(reader: &mut impl Read, output: &mut Vec<u8>) -> io::Result<()> {
    let mut bytes = [0_u8; 1024];
    loop {
        match reader.read(&mut bytes) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                if output.len() + count > OUTPUT_LIMIT {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "scheduler response exceeds limit",
                    ));
                }
                output.extend_from_slice(&bytes[..count]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn run(command: &mut Command, deadline: Instant, cancelled: &AtomicBool) -> io::Result<Vec<u8>> {
    check_budget(deadline, cancelled)?;
    // Stable diagnostics are inspected for typed manager denials, never exposed
    // to callers or logs (they may contain manager-provided command metadata).
    command.env("LC_ALL", "C");
    let stdio = SpawnStdio {
        stdout: StdioSource::Pipe,
        stderr: StdioSource::Pipe,
        drain_timeout: None,
        ..SpawnStdio::default()
    };
    let mut child =
        crate::spawn_sync(command, stdio, SyncEnvironment::Inherit).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "user service manager command is unavailable",
                )
            } else {
                error
            }
        })?;
    let mut stdout = child.stdout.take().expect("configured stdout pipe");
    let mut stderr = child.stderr.take().expect("configured stderr pipe");
    nonblocking(&stdout)?;
    nonblocking(&stderr)?;
    let mut output = Vec::new();
    let mut diagnostic = Vec::new();
    loop {
        check_budget(deadline, cancelled)?;
        drain(&mut stdout, &mut output)?;
        drain(&mut stderr, &mut diagnostic)?;
        if let Some(code) = child.try_wait()? {
            drain(&mut stdout, &mut output)?;
            drain(&mut stderr, &mut diagnostic)?;
            if code == 0 {
                return Ok(output);
            }
            let diagnostic = String::from_utf8_lossy(&diagnostic).to_ascii_lowercase();
            let kind = if [
                "permission denied",
                "access denied",
                "interactive authentication required",
            ]
            .iter()
            .any(|text| diagnostic.contains(text))
            {
                io::ErrorKind::PermissionDenied
            } else if ["failed to connect", "unrecognized option", "unknown option"]
                .iter()
                .any(|text| diagnostic.contains(text))
            {
                io::ErrorKind::Unsupported
            } else if diagnostic.contains("not loaded") || diagnostic.contains("could not be found")
            {
                io::ErrorKind::NotFound
            } else {
                io::ErrorKind::Other
            };
            return Err(io::Error::new(
                kind,
                "user service manager rejected the operation",
            ));
        }
        std::thread::sleep(
            Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn check_budget(deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
    if cancelled.load(Ordering::Acquire) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "scheduler launch cancelled",
        ))
    } else if Instant::now() >= deadline {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "scheduler launch deadline expired",
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_receives_only_helper_and_private_payload_path() {
        let command = launch_command(
            "fixture.service",
            Path::new("/opt/helper"),
            Path::new("/run/user/1000/private/request"),
        )
        .unwrap();
        let args: Vec<_> = command.get_args().collect();
        assert!(args.contains(&std::ffi::OsStr::new("--service-type=exec")));
        assert!(args.contains(&std::ffi::OsStr::new("--expand-environment=no")));
        assert!(!args.contains(&std::ffi::OsStr::new("--scope")));
        assert_eq!(
            &args[args.len() - 2..],
            ["/opt/helper", "/run/user/1000/private/request"]
        );
    }

    #[test]
    fn invalid_paths_fail_before_scheduling() {
        assert!(launch_command(
            "fixture.service",
            Path::new("helper"),
            Path::new("/request")
        )
        .is_err());
    }

    #[test]
    fn cancellation_and_timeout_are_distinct() {
        assert_eq!(
            check_budget(
                Instant::now() + Duration::from_secs(1),
                &AtomicBool::new(true)
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(
            check_budget(Instant::now(), &AtomicBool::new(false))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn manager_denial_is_typed_and_diagnostics_do_not_leak() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf 'Access denied: secret-payload' >&2; exit 1"]);
        let error = run(
            &mut command,
            Instant::now() + Duration::from_secs(2),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!error.to_string().contains("secret-payload"));
    }

    #[test]
    fn missing_manager_is_unsupported() {
        let directory = tempfile::tempdir().unwrap();
        let mut command = Command::new(directory.path().join("absent-manager"));
        let error = run(
            &mut command,
            Instant::now() + Duration::from_secs(2),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn unresponsive_command_is_bounded() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 60"]);
        let start = Instant::now();
        let error = run(
            &mut command,
            start + Duration::from_millis(30),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn oversized_command_output_is_rejected() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "head -c 16384 /dev/zero"]);
        let error = run(
            &mut command,
            Instant::now() + Duration::from_secs(2),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    #[ignore = "requires a running systemd user manager and cgroup-v2 worker scope"]
    fn real_service_is_outside_worker_and_stop_reaps_it() {
        let directory = tempfile::tempdir().unwrap();
        let request = directory.path().join("request");
        std::fs::write(&request, "exec sleep 60\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let cancelled = AtomicBool::new(false);
        let mut unit =
            ScheduledUnit::launch(Path::new("/bin/sh"), &request, deadline, &cancelled).unwrap();
        let worker =
            super::super::resource_placement::Placement::capture(std::process::id()).unwrap();
        let live = unit
            .verify_helper_placement(&worker, deadline, &cancelled)
            .unwrap();
        assert!(live.is_alive());
        unit.stop(deadline).unwrap();
        assert!(!live.is_alive());
    }

    #[test]
    #[ignore = "requires a running systemd user manager"]
    fn rollback_drop_stops_uncommitted_service() {
        let directory = tempfile::tempdir().unwrap();
        let request = directory.path().join("request");
        std::fs::write(&request, "exec sleep 60\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let cancelled = AtomicBool::new(false);
        let unit =
            ScheduledUnit::launch(Path::new("/bin/sh"), &request, deadline, &cancelled).unwrap();
        let live = super::super::process_inspect::ProcessLiveness::open(
            unit.main_pid(deadline, &cancelled).unwrap(),
        )
        .unwrap();
        drop(unit);
        assert!(!live.is_alive());
    }

    #[test]
    #[ignore = "requires a running systemd user manager"]
    fn committed_service_survives_handle_drop_until_explicit_stop() {
        let directory = tempfile::tempdir().unwrap();
        let request = directory.path().join("request");
        std::fs::write(&request, "exec sleep 60\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let cancelled = AtomicBool::new(false);
        let mut unit =
            ScheduledUnit::launch(Path::new("/bin/sh"), &request, deadline, &cancelled).unwrap();
        let mut cleanup = ScheduledUnit {
            name: unit.name.clone(),
            cleanup_on_drop: true,
        };
        let live = super::super::process_inspect::ProcessLiveness::open(
            unit.main_pid(deadline, &cancelled).unwrap(),
        )
        .unwrap();
        unit.retain_on_drop();
        drop(unit);
        assert!(live.is_alive());
        cleanup.stop(deadline).unwrap();
        assert!(!live.is_alive());
    }
}
