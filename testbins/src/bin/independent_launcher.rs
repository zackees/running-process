//! Caller-lifetime fixture. The harness may put this process in a newly owned
//! cgroup after `launcher-started` and before creating `launch`. Only then is
//! the memory-holder daemon created. No cgroup paths are accepted or modified
//! here: the harness owns placement and teardown authority.

use running_process::{
    spawn_daemon_request, DaemonSpawnRequest, IndependentSpawnOptions, SpawnMode,
};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

fn mark(directory: &Path, name: &str, pid: u32) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join(name))?;
    writeln!(file, "{pid}")?;
    file.sync_all()
}

fn wait_for(directory: &Path, name: &str) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !directory.join(name).try_exists()? {
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, name.to_owned()));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let directory = args.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "private fixture directory required",
        )
    })?;
    let mode = match args.next().as_deref().and_then(|arg| arg.to_str()) {
        Some("inherited") => SpawnMode::Inherited,
        Some("independent") => SpawnMode::Independent,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "explicit spawn mode required",
            ))
        }
    };
    let directory = Path::new(&directory);
    mark(directory, "launcher-started", std::process::id())?;
    wait_for(directory, "launch")?;
    let executable = std::env::current_exe()?.with_file_name(format!(
        "testbin-independent-memory-holder{}",
        std::env::consts::EXE_SUFFIX
    ));
    let mut request = DaemonSpawnRequest::new(executable);
    request.arg(directory);
    let mut child = match spawn_daemon_request(
        &mut request,
        &IndependentSpawnOptions {
            mode,
            ..Default::default()
        },
    ) {
        Ok(child) => child,
        Err(error) => {
            use running_process::IndependentSpawnError;
            // Fixed categories only: no backend resource paths or command
            // environment values enter the fixture diagnostic channel.
            let category = match error {
                IndependentSpawnError::Unsupported { .. } => 1,
                IndependentSpawnError::PermissionDenied { .. } => 2,
                IndependentSpawnError::Launch { .. } => 3,
                IndependentSpawnError::Readiness { .. } => 4,
                IndependentSpawnError::Cancelled => 5,
                IndependentSpawnError::CleanupUnconfirmed { .. } => 6,
            };
            mark(directory, "launcher-failed", category)?;
            return Err(io::Error::other("fixture daemon launch failed"));
        }
    };
    let result =
        mark(directory, "launched", child.id()).and_then(|()| wait_for(directory, "exit-launcher"));
    if result.is_err() {
        let _ = child.kill();
    }
    // Successful exit deliberately relinquishes this non-killing handle.
    // The daemon has its own deadlines if the external harness disappears.
    result
}
