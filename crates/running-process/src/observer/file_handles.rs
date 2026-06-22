//! #539 — snapshot the file handles held by any LaunchedProcessTree
//! PID without admin privileges.
//!
//! Cross-platform dispatcher matching the [`super::cmdline`] shape:
//!
//! - **Linux**: walk `/proc/<pid>/fd/*` and `readlink()` each entry.
//!   Anonymous handles (`socket:[...]`, `pipe:[...]`, `anon_inode:...`)
//!   are returned as opaque labels alongside real filesystem paths.
//! - **macOS**: `proc_pidinfo(pid, PROC_PIDLISTFDS, ...)` enumerates
//!   the fd table, then `proc_pidinfo(pid, PROC_PIDFDVNODEPATHINFO,
//!   fd, ...)` resolves each vnode-backed fd to its filesystem path.
//!   Sockets / pipes / kqueues without a path are skipped.
//! - **Windows**: deferred to slice 4 of #539 (NtQuerySystemInformation
//!   handle snapshot + DuplicateHandle + NtQueryObject — substantially
//!   more involved than the Unix paths). Returns
//!   [`ErrorKind::Unsupported`] with the slice anchor in the message.

/// Snapshot the file handles currently held by `pid`, returned as
/// human-readable strings (filesystem paths where possible,
/// `socket:[...]` / `anon_inode:...` style labels otherwise).
///
/// The list is best-effort and racy by nature — handles open and
/// close between the enumeration call and the per-fd lookup. Any fd
/// that disappears mid-walk is silently skipped rather than failing
/// the whole snapshot.
pub fn read_process_file_handles(pid: u32) -> std::io::Result<Vec<String>> {
    #[cfg(target_os = "linux")]
    {
        linux_impl::read_process_file_handles(pid)
    }
    #[cfg(target_os = "macos")]
    {
        macos_impl::read_process_file_handles(pid)
    }
    #[cfg(target_os = "windows")]
    {
        let _ = pid;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Windows NtQuerySystemInformation handle snapshot backend not yet implemented (#539 slice 4)",
        ))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = pid;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "#539: no LaunchedProcessTree handle-snapshot backend planned for this OS",
        ))
    }
}

#[cfg(target_os = "linux")]
mod linux_impl {
    pub(super) fn read_process_file_handles(pid: u32) -> std::io::Result<Vec<String>> {
        if pid == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "pid 0 is the kernel scheduler — not queryable",
            ));
        }
        let dir = format!("/proc/{pid}/fd");
        let entries = std::fs::read_dir(&dir)?;
        let mut handles = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else { continue };
            // Each entry is a symlink to either a filesystem path
            // (e.g. /etc/hosts) or an anonymous kernel object
            // (`socket:[12345]`, `pipe:[67890]`, `anon_inode:...`).
            // `read_link` returns the target as a PathBuf — keep the
            // raw lossy-decoded string so anonymous targets survive
            // intact for downstream pattern-matching.
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            handles.push(target.to_string_lossy().into_owned());
        }
        Ok(handles)
    }
}

#[cfg(target_os = "macos")]
mod macos_impl {
    // libc 0.2 exposes `proc_pidinfo` / `proc_pidfdinfo` and the
    // `proc_fdinfo` / `vnode_fdinfowithpath` structs on macOS but
    // does NOT export the integer flavor constants — declare them
    // inline from `<sys/proc_info.h>`. ABI-stable.
    const PROC_PIDLISTFDS: libc::c_int = 1;
    const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;
    const PROX_FDTYPE_VNODE: u32 = 1;

    pub(super) fn read_process_file_handles(pid: u32) -> std::io::Result<Vec<String>> {
        if pid == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "pid 0 is the kernel scheduler — not queryable",
            ));
        }
        // Size probe: PROC_PIDLISTFDS with null buffer returns required
        // bytes for the proc_fdinfo array.
        let size = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                PROC_PIDLISTFDS,
                0,
                std::ptr::null_mut(),
                0,
            )
        };
        if size <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        let entry_size = std::mem::size_of::<libc::proc_fdinfo>();
        let count = (size as usize) / entry_size;
        let mut fds: Vec<libc::proc_fdinfo> = vec![unsafe { std::mem::zeroed() }; count];
        let written = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr() as *mut libc::c_void,
                (count * entry_size) as libc::c_int,
            )
        };
        if written <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        let written_count = (written as usize) / entry_size;
        fds.truncate(written_count);

        let mut handles = Vec::new();
        for fd in &fds {
            // We only resolve vnode-backed fds (regular files,
            // directories, devices). Sockets/pipes/kqueues have no
            // POSIX path; skip them.
            if fd.proc_fdtype != PROX_FDTYPE_VNODE {
                continue;
            }
            let mut info: libc::vnode_fdinfowithpath = unsafe { std::mem::zeroed() };
            let n = unsafe {
                libc::proc_pidfdinfo(
                    pid as libc::c_int,
                    fd.proc_fd,
                    PROC_PIDFDVNODEPATHINFO,
                    &mut info as *mut libc::vnode_fdinfowithpath as *mut libc::c_void,
                    std::mem::size_of::<libc::vnode_fdinfowithpath>() as libc::c_int,
                )
            };
            if n <= 0 {
                // fd closed between listfds and fdinfo — skip the race.
                continue;
            }
            // `pvip.vip_path` is `c_char[MAXPATHLEN]` (1024). Find the
            // NUL terminator and decode lossy UTF-8.
            let path_bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    info.pvip.vip_path.as_ptr() as *const u8,
                    info.pvip.vip_path.len(),
                )
            };
            let nul = path_bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(path_bytes.len());
            if nul == 0 {
                continue;
            }
            let path = String::from_utf8_lossy(&path_bytes[..nul]).into_owned();
            handles.push(path);
        }
        Ok(handles)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn read_handles_for_pid_zero_returns_invalid_input() {
        let err = read_process_file_handles(0).expect_err("pid 0 should be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_handles_returns_unsupported_pointing_at_slice_4() {
        let err = read_process_file_handles(std::process::id())
            .expect_err("windows backend deferred");
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
        assert!(
            err.to_string().contains("#539 slice 4"),
            "Unsupported reason must anchor to slice 4: {err}"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn self_snapshot_includes_a_temp_file_we_just_opened() {
        // Open a temp file, snapshot our own fds, assert the temp
        // file's path is in the result. Works on both Linux (via
        // /proc/self/fd/*) and macOS (via proc_pidinfo on self).
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        let canonical = std::fs::canonicalize(&path).unwrap_or(path.clone());

        let handles = read_process_file_handles(std::process::id())
            .expect("read self handles");
        let canonical_str = canonical.to_string_lossy();
        let raw_str = path.to_string_lossy();
        let found = handles
            .iter()
            .any(|h| h == canonical_str.as_ref() || h == raw_str.as_ref());
        assert!(
            found,
            "expected temp file {canonical_str} (or {raw_str}) in handles, got {handles:?}",
        );
        // Drop tmp explicitly so it stays alive until after the snapshot.
        drop(tmp);
    }
}
