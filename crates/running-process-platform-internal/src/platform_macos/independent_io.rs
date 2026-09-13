//! Scheduler log/readiness files cannot be pipes, devices or symlinks.
use std::{
    fs::{File, OpenOptions},
    io,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub(crate) fn open_regular(path: &Path, append: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(!append)
        .append(append)
        .create(append)
        .mode(0o600)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| match error.raw_os_error() {
            Some(libc::ENXIO | libc::ELOOP | libc::EISDIR) => io::Error::new(
                io::ErrorKind::Unsupported,
                "independent launch requires a regular file",
            ),
            _ => error,
        })?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "independent launch requires a regular file",
        ));
    }
    Ok(file)
}
