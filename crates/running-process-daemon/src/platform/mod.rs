#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

use std::path::Path;

/// Write the current process ID to the given PID file, creating parent
/// directories as needed.
pub fn write_pid_file(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, std::process::id().to_string())
}

/// Read a PID from the given file, returning `None` if the file is missing
/// or its contents are not a valid `u32`.
pub fn read_pid_file(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Remove the PID file, ignoring errors (e.g. if it does not exist).
pub fn remove_pid_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Check whether a process with the given PID is currently alive.
pub fn is_process_alive(pid: u32) -> bool {
    use sysinfo::System;

    let pid = sysinfo::Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_process(pid);
    sys.process(pid).is_some()
}
