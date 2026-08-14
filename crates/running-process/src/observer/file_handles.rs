//! Portable facade for native file-handle snapshots.

pub fn read_process_file_handles(pid: u32) -> std::io::Result<Vec<String>> {
    running_process_platform_internal::platform::process::read_process_file_handles(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_handles_for_pid_zero_returns_invalid_input() {
        let err = read_process_file_handles(0).expect_err("pid 0 should be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn self_snapshot_includes_a_temp_file_we_just_opened() {
        // Open a temp file, snapshot our own fds, assert the temp
        // file's path is in the result.
        //
        // Unix backends return POSIX paths that match `path` /
        // `canonical` directly. The Windows backend returns NT
        // object names like `\Device\HarddiskVolume3\Users\...\tmpXXXX`
        // which won't match a DOS-style path equal-for-equal â€” so we
        // also match on filename suffix, which is reliable across
        // the NT/DOS path translation gap.
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        let canonical = std::fs::canonicalize(&path).unwrap_or(path.clone());
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
            .unwrap_or_default();

        let handles = read_process_file_handles(std::process::id()).expect("read self handles");
        let canonical_str = canonical.to_string_lossy();
        let raw_str = path.to_string_lossy();
        let found = handles.iter().any(|h| {
            h == canonical_str.as_ref()
                || h == raw_str.as_ref()
                || (!filename.is_empty() && h.ends_with(&filename))
        });
        assert!(
            found,
            "expected temp file (filename={filename}, canonical={canonical_str}, raw={raw_str}) in handles, got {handles:?}",
        );
        // Drop tmp explicitly so it stays alive until after the snapshot.
        drop(tmp);
    }
}
