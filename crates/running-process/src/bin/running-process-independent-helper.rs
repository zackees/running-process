//! One-shot helper run by an already-independent scheduler/broker.
#[cfg(target_os = "linux")]
fn main() -> std::io::Result<()> {
    use std::io::Write as _;
    let mut args = std::env::args_os();
    let _ = args.next();
    let request = args.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing request path")
    })?;
    if request == "--broker" {
        let socket = args.next().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing broker socket")
        })?;
        if args.next().is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "unexpected broker arguments",
            ));
        }
        return running_process::independent_transport_for_helper::serve_broker(
            std::path::Path::new(&socket),
        );
    }
    let acknowledgement = std::path::PathBuf::from(args.next().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "missing acknowledgement path",
        )
    })?);
    let acceptance_timeout = args
        .next()
        .and_then(|value| value.to_str().and_then(|value| value.parse::<u64>().ok()))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "missing acceptance timeout",
            )
        })?;
    if args.next().is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unexpected helper arguments",
        ));
    }
    let request_path = std::path::Path::new(&request);
    if !request_path.is_absolute()
        || acknowledgement != request_path.with_file_name("ack")
        || acceptance_timeout == 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid helper protocol paths or timeout",
        ));
    }
    if private_marker_exists(&acknowledgement.with_file_name("cancel"))? {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(acknowledgement.with_file_name("cancelled"))?
            .sync_all()?;
        return Ok(());
    }
    let supervisor_pid = std::process::id();
    let supervisor_key = running_process::independent_transport_for_helper::current_start_key()?;
    let request = running_process::independent_transport_for_helper::read_and_spawn(&request)?;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&acknowledgement)
    {
        Ok(file) => file,
        Err(error) => {
            request.cleanup();
            return Err(error);
        }
    };
    if let Err(error) = writeln!(
        file,
        "{} {} {} {}",
        request.pid, request.start_key, supervisor_pid, supervisor_key
    )
    .and_then(|_| file.sync_all())
    {
        request.cleanup();
        return Err(error);
    }
    // Do not reap yet: even a short-lived child's PID/start identity must
    // remain available while its caller verifies placement and opens pidfds.
    let accepted = acknowledgement.with_file_name("accepted");
    let started = std::time::Instant::now();
    loop {
        let cancelled = match private_marker_exists(&acknowledgement.with_file_name("cancel")) {
            Ok(value) => value,
            Err(error) => {
                request.cleanup();
                return Err(error);
            }
        };
        if cancelled {
            request.cleanup_confirmed()?;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(acknowledgement.with_file_name("cancelled"))?
                .sync_all()?;
            return Ok(());
        }
        match private_marker_exists(&accepted) {
            Ok(true) => break,
            Ok(false) => {}
            Err(error) => {
                request.cleanup();
                return Err(error);
            }
        }
        if started.elapsed() >= std::time::Duration::from_millis(acceptance_timeout) {
            // Publish the same positive cleanup acknowledgement as explicit
            // cancellation. A caller cancelling after this timeout can then
            // distinguish a reaped target from an unresponsive supervisor.
            request.cleanup_confirmed()?;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(acknowledgement.with_file_name("cancelled"))?
                .sync_all()?;
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "caller did not accept launch",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let status = request.wait()?;
    let mut status_path = acknowledgement.clone().into_os_string();
    status_path.push(".status");
    let temporary = std::path::PathBuf::from({
        let mut value = status_path.clone();
        value.push(".tmp");
        value
    });
    let mut status_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)?;
    status_file.write_all(status.to_string().as_bytes())?;
    status_file.sync_all()?;
    std::fs::rename(temporary, std::path::PathBuf::from(status_path))
}

/// Inspect a marker through a held file descriptor, without following links
/// or blocking on a substituted FIFO. Protocol markers contain no payload.
#[cfg(target_os = "linux")]
fn private_marker_exists(path: &std::path::Path) -> std::io::Result<bool> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    // SAFETY: geteuid takes no pointers and only returns the caller identity.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
        || metadata.len() != 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe helper marker",
        ));
    }
    Ok(true)
}

#[cfg(all(test, target_os = "linux"))]
mod marker_tests {
    use super::private_marker_exists;
    use std::os::unix::fs::{symlink, OpenOptionsExt, PermissionsExt};

    #[test]
    fn only_private_empty_regular_markers_are_accepted() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("accepted");
        assert!(!private_marker_exists(&marker).unwrap());
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&marker)
            .unwrap();
        assert!(private_marker_exists(&marker).unwrap());
        file.set_len(1).unwrap();
        assert!(private_marker_exists(&marker).is_err());
        file.set_len(0).unwrap();
        file.set_permissions(std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(private_marker_exists(&marker).is_err());
    }

    #[test]
    fn linked_markers_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("cancel");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&marker)
            .unwrap();
        let alias = directory.path().join("alias");
        symlink(&marker, &alias).unwrap();
        assert!(private_marker_exists(&alias).is_err());
        let hard_link = directory.path().join("hard-link");
        std::fs::hard_link(&marker, &hard_link).unwrap();
        assert!(private_marker_exists(&marker).is_err());
        assert!(private_marker_exists(directory.path()).is_err());
    }
}
#[cfg(target_os = "windows")]
fn main() -> std::io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "expected request, acknowledgement, timeout",
        )
    };
    let request = args.next().ok_or_else(invalid)?;
    let acknowledgement = args.next().ok_or_else(invalid)?;
    let timeout = args
        .next()
        .and_then(|value| value.to_str().and_then(|value| value.parse::<u64>().ok()))
        .ok_or_else(invalid)?;
    if args.next().is_some() {
        return Err(invalid());
    }
    running_process::independent_windows_helper::run(
        std::path::Path::new(&request),
        std::path::Path::new(&acknowledgement),
        timeout,
    )
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn main() -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this helper build has no independent-launch backend for this platform",
    ))
}
