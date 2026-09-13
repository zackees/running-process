//! Windows half of the private helper acceptance protocol.
use running_process_platform_internal::{
    create_private_launch_file, open_private_launch_file, ProcessLiveness,
};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

pub fn run(request_path: &Path, acknowledgement: &Path, timeout_ms: u64) -> io::Result<()> {
    if !request_path.is_absolute()
        || acknowledgement != request_path.with_file_name("ack")
        || timeout_ms == 0
        || timeout_ms > 3_600_000
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid helper protocol arguments",
        ));
    }
    let directory = running_process_platform_internal::open_private_launch_directory(
        request_path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "request has no parent"))?,
    )?;
    // A scheduler action may begin after the caller's cancellation deadline.
    // Confirm the empty launch before reading or executing its target request.
    if marker_exists(&request_path.with_file_name("cancel"))? {
        return write_completion(&request_path.with_file_name("cancelled"), b"", directory);
    }
    let supervisor = ProcessLiveness::open(std::process::id()).map_err(|error| error.source)?;
    let supervisor_key = supervisor.creation_time()?;
    let request = crate::independent_transport::read_private_request(request_path)?;
    if marker_exists(&request_path.with_file_name("cancel"))? {
        return write_completion(&request_path.with_file_name("cancelled"), b"", directory);
    }
    let mut child = crate::independent_transport::spawn_request(request)?;
    let result = (|| -> io::Result<i32> {
        // Child owns a process handle even if the process exits immediately,
        // so the identity cannot be reused during caller acknowledgement.
        let identity = ProcessLiveness::open(child.id()).map_err(|error| error.source)?;
        let key = identity.creation_time()?;
        write_artifact(
            acknowledgement,
            format!(
                "{} {} {} {}",
                child.id(),
                key,
                std::process::id(),
                supervisor_key
            )
            .as_bytes(),
        )?;
        let started = Instant::now();
        loop {
            if marker_exists(&request_path.with_file_name("cancel"))? {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "caller cancelled launch",
                ));
            }
            if marker_exists(&request_path.with_file_name("accepted"))? {
                break;
            }
            if started.elapsed() >= Duration::from_millis(timeout_ms) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "caller did not accept launch",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(child.wait()?.code().unwrap_or(1))
    })();
    let status = match result {
        Ok(status) => status,
        Err(error) => {
            // Never positively acknowledge cleanup before termination is observed.
            if child.try_wait()?.is_none() {
                child.kill()?;
                child.wait()?;
            }
            write_completion(&request_path.with_file_name("cancelled"), b"", directory)?;
            return Err(error);
        }
    };
    write_completion(
        &request_path.with_file_name("ack.status"),
        status.to_string().as_bytes(),
        directory,
    )
}

/// Release the directory lock before making final status readable. Otherwise
/// a caller can observe success but lose its chance to remove the directory.
fn write_completion(path: &Path, bytes: &[u8], directory: std::fs::File) -> io::Result<()> {
    let mut file = create_private_launch_file(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(directory);
    // Publication: the exclusive file becomes readable only after this drop.
    drop(file);
    Ok(())
}

fn write_artifact(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = create_private_launch_file(path)?;
    file.write_all(bytes)?;
    file.sync_all()
    // Closing the exclusive writer publishes a complete artifact to readers.
}

fn marker_exists(path: &Path) -> io::Result<bool> {
    let file = match open_private_launch_file(path) {
        Ok(file) => file,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(32) =>
        {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "helper marker contains unexpected payload",
        ));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_cancellation_is_acknowledged_without_reading_request() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("private");
        running_process_platform_internal::create_private_launch_directory(&root).unwrap();
        let request = root.join("request");
        write_artifact(&root.join("cancel"), b"").unwrap();
        // Deliberately no request file: attempting to read or spawn it fails.
        run(&request, &root.join("ack"), 1000).unwrap();
        assert!(marker_exists(&root.join("cancelled")).unwrap());
        assert!(!root.join("ack").exists());
    }

    #[test]
    fn mismatched_acknowledgement_path_is_rejected_before_launch() {
        assert_eq!(
            run(
                Path::new(r"C:\private\request"),
                Path::new(r"C:\other\ack"),
                1000
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
