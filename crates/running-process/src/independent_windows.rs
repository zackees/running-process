//! Windows scheduler placement and caller-side acceptance coordination.
use super::{
    cancelled, DaemonChild, IndependentSpawnBackend, IndependentSpawnCapability,
    IndependentSpawnError, IndependentSpawnOptions,
};
use running_process_platform_internal::platform::process::DaemonChildControl;
use running_process_platform_internal::{
    create_private_launch_file, open_private_launch_file, verify_outside_current_job,
    IndependentSchedulerTask, ProcessLiveness,
};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

fn helper_path() -> io::Result<PathBuf> {
    let path = match crate::env_vars::INDEPENDENT_HELPER.path() {
        Some(path) => path,
        None => std::env::current_exe()?.with_file_name("running-process-independent-helper.exe"),
    }
    .canonicalize()?;
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "independent helper is not a file",
        ));
    }
    Ok(path)
}

pub(super) fn capability() -> IndependentSpawnCapability {
    let result = helper_path().and_then(|_| {
        IndependentSchedulerTask::probe(
            Instant::now() + Duration::from_secs(2),
            &AtomicBool::new(false),
        )
    });
    IndependentSpawnCapability { available: result.is_ok(),
        backend: Some(IndependentSpawnBackend::WindowsTaskScheduler),
        reason: match result {
            Ok(()) => "helper installed, interactive token present, scheduler reachable; registration and placement are verified at launch".into(),
            Err(error) => error.to_string(),
        } }
}

pub(super) fn spawn(
    command: &mut Command,
    options: &IndependentSpawnOptions,
) -> Result<DaemonChild, IndependentSpawnError> {
    let started = Instant::now();
    let deadline = started
        .checked_add(options.readiness_timeout)
        .ok_or_else(|| IndependentSpawnError::Unsupported {
            reason: "launch deadline exceeds clock range".into(),
        })?;
    check(options, deadline)?;
    running_process_platform_internal::require_interactive_scheduler_token().map_err(classify)?;
    let helper = helper_path().map_err(classify)?;
    let request = crate::independent_transport::LaunchRequest::from_command(
        command,
        options.inherit_environment,
    )
    .map_err(classify)?;
    let request_path =
        crate::independent_transport::write_private_request(&request).map_err(classify)?;
    let directory = running_process_platform_internal::open_private_launch_directory(
        request_path
            .parent()
            .ok_or_else(|| classify(io::Error::other("request has no parent")))?,
    )
    .map_err(classify)?;
    let ack = request_path.with_file_name("ack");
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let task = IndependentSchedulerTask::new(format!(
        "running-process-independent-{}-{unique}",
        std::process::id()
    ))
    .map_err(classify)?;
    let fallback = AtomicBool::new(false);
    let cancellation = options
        .cancellation
        .as_ref()
        .map(|token| token.0.as_ref())
        .unwrap_or(&fallback);
    let mut registered = false;
    let mut attempted_registration = false;
    let mut removed = false;
    let result = (|| {
        let xml = task
            .render_xml(
                &helper,
                &request_path,
                &ack,
                options
                    .readiness_timeout
                    .as_millis()
                    .saturating_add(1000)
                    .min(3_600_000) as u64,
            )
            .map_err(classify)?;
        let xml_path = request_path.with_file_name("task.xml");
        write_artifact(&xml_path, xml.as_bytes()).map_err(classify)?;
        check(options, deadline)?;
        attempted_registration = true;
        task.register(&xml_path, deadline, cancellation)
            .map_err(classify)?;
        registered = true;
        task.run(deadline, cancellation).map_err(classify)?;
        loop {
            check(options, deadline)?;
            let identities = read_control(&ack)
                .map_err(classify)?
                .map(|text| super::parse_launch_acknowledgement(&text))
                .transpose()
                .map_err(classify)?
                .flatten();
            if let Some((pid, key, supervisor_pid, supervisor_key)) = identities {
                let target = verify_outside_current_job(pid, key)
                    .map_err(|error| classify(error.source))?
                    .liveness;
                let supervisor = verify_outside_current_job(supervisor_pid, supervisor_key)
                    .map_err(|error| classify(error.source))?
                    .liveness;
                // Deleting the on-demand registration does not terminate its
                // running action; remove persistence before accepting ownership.
                task.delete(deadline, cancellation).map_err(classify)?;
                removed = true;
                // The verified helper acknowledgement proves consumption.
                // Do not retain target environment/argv until daemon exit or
                // depend on the caller eventually waiting on its child handle.
                std::fs::remove_file(&request_path).map_err(classify)?;
                std::fs::remove_file(&xml_path).map_err(classify)?;
                check(options, deadline)?;
                let accepted = request_path.with_file_name("accepted");
                let temporary = request_path.with_file_name("accepted.tmp");
                let marker = create_private_launch_file(&temporary).map_err(classify)?;
                marker.sync_all().map_err(classify)?;
                drop(marker);
                check(options, deadline)?;
                if supervisor.has_exited().map_err(classify)? {
                    return Err(IndependentSpawnError::Launch {
                        reason: "independent supervisor exited before launch acceptance".into(),
                    });
                }
                // Publish only after the final cancellation/deadline check.
                // No fallible operation follows acceptance before returning.
                std::fs::rename(&temporary, &accepted).map_err(classify)?;
                return Ok(DaemonChild::from_external(
                    pid,
                    Box::new(WindowsChild {
                        target,
                        supervisor,
                        status: request_path.with_file_name("ack.status"),
                        cached: None,
                    }),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    drop(directory);
    match result {
        Ok(child) => Ok(child),
        Err(cause) => {
            let resource = task.name().to_owned();
            if attempted_registration && !registered {
                return Err(IndependentSpawnError::CleanupUnconfirmed { cause: Box::new(cause), resource,
                    reason: "registration outcome is unknown; retained private request for reconciliation".into() });
            }
            let cleanup = (|| -> io::Result<()> {
                if registered {
                    write_artifact(&request_path.with_file_name("cancel"), b"")?;
                    let cleanup_deadline = Instant::now() + Duration::from_secs(2);
                    if !removed {
                        task.delete(cleanup_deadline, &AtomicBool::new(false))?;
                    }
                    loop {
                        if super::cleanup_acknowledged(read_control(
                            &request_path.with_file_name("cancelled"),
                        )?)? {
                            break;
                        }
                        if Instant::now() >= cleanup_deadline {
                            return Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "target cleanup was not acknowledged",
                            ));
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                remove_artifacts(&request_path);
                Ok(())
            })();
            match cleanup {
                Ok(()) => Err(cause),
                Err(error) => Err(IndependentSpawnError::CleanupUnconfirmed {
                    cause: Box::new(cause),
                    resource,
                    reason: error.to_string(),
                }),
            }
        }
    }
}

fn check(
    options: &IndependentSpawnOptions,
    deadline: Instant,
) -> Result<(), IndependentSpawnError> {
    if cancelled(options) {
        return Err(IndependentSpawnError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(IndependentSpawnError::Readiness {
            timeout: options.readiness_timeout,
            reason: "Windows independent launch deadline elapsed".into(),
        });
    }
    Ok(())
}
fn classify(error: io::Error) -> IndependentSpawnError {
    if error.kind() == io::ErrorKind::Unsupported {
        IndependentSpawnError::Unsupported {
            reason: error.to_string(),
        }
    } else if error.kind() == io::ErrorKind::PermissionDenied {
        IndependentSpawnError::PermissionDenied {
            reason: error.to_string(),
        }
    } else {
        IndependentSpawnError::Launch {
            reason: error.to_string(),
        }
    }
}
fn write_artifact(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = create_private_launch_file(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}
fn read_control(path: &Path) -> io::Result<Option<String>> {
    let file = match open_private_launch_file(path) {
        Ok(file) => file,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(32) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error),
    };
    let mut text = String::new();
    file.take(129).read_to_string(&mut text)?;
    if text.len() > 128 {
        return Err(io::Error::other("oversized helper control artifact"));
    }
    Ok(Some(text))
}
fn remove_artifacts(request: &Path) {
    if let Some(root) = request.parent() {
        for name in [
            "request",
            "ack",
            "accepted",
            "accepted.tmp",
            "cancel",
            "cancelled",
            "ack.status",
            "task.xml",
        ] {
            let _ = std::fs::remove_file(root.join(name));
        }
        let _ = std::fs::remove_dir(root);
    }
}
struct WindowsChild {
    target: ProcessLiveness,
    supervisor: ProcessLiveness,
    status: PathBuf,
    cached: Option<i32>,
}
impl DaemonChildControl for WindowsChild {
    fn kill(&mut self) -> io::Result<()> {
        self.target.kill()
    }
    fn wait(&mut self) -> io::Result<i32> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        if self.cached.is_some() {
            return Ok(self.cached);
        }
        let mut text = read_control(&self.status)?;
        if text.is_none() && self.supervisor.has_exited()? {
            text = read_control(&self.status)?;
            if text.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "helper exited without target status",
                ));
            }
        }
        if let Some(text) = text {
            let status = text
                .parse()
                .map_err(|_| io::Error::other("invalid helper exit status"))?;
            self.cached = Some(status);
            remove_artifacts(&self.status);
        }
        Ok(self.cached)
    }
}
