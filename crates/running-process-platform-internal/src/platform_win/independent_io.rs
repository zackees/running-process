//! Scheduler log/readiness files cannot be pipes, devices or reparse points.
use std::{
    fs::{File, OpenOptions},
    io,
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
};

pub(crate) fn open_regular(path: &Path, append: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(!append)
        .append(append)
        .create(append)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "independent launch requires a regular file",
        ));
    }
    Ok(file)
}
