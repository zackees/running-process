//! Portable facade for native command-line inspection.

pub fn read_process_cmdline(pid: u32) -> std::io::Result<String> {
    running_process_platform_internal::platform::process::read_process_cmdline(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    #[test]
    fn read_cmdline_for_pid_zero_returns_invalid_input() {
        // PID 0 is the system idle process on Windows / kernel scheduler
        // on Linux + macOS â€” not openable from user-mode on any of them,
        // so all three backends reject it up front before touching FFI /
        // FS.
        let err = read_process_cmdline(0).expect_err("pid 0 should be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn unix_read_cmdline_round_trips_known_args_from_spawned_child() {
        use crate::observer::ObserverConfig;
        use crate::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};
        use std::time::Duration;

        // Long-lived `sleep 30` (available on both Linux and macOS as a
        // POSIX standard utility) with a distinctive argv: read it
        // back via the per-OS no-admin primitive while the child is
        // still alive.
        let cfg = ProcessConfig {
            command: CommandSpec::Argv(vec!["sleep".into(), "30".into()]),
            cwd: None,
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            address_space_limit_bytes: None,
        };
        let (process, _sub) = NativeProcess::with_observer(cfg, ObserverConfig::lifecycle());
        process.start().expect("spawn sleep");
        let pid = process.pid().expect("pid");
        std::thread::sleep(Duration::from_millis(100));

        let cmdline = read_process_cmdline(pid).expect("read cmdline");
        process.kill().ok();
        process.close().ok();

        assert!(
            cmdline.contains("sleep"),
            "expected 'sleep' in cmdline, got: {cmdline:?}"
        );
        assert!(
            cmdline.contains("30"),
            "expected '30' (the sleep duration) in cmdline, got: {cmdline:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_read_cmdline_for_nonexistent_pid_returns_not_found() {
        let err = read_process_cmdline(0x7FFF_FFFE).expect_err("nonexistent pid");
        // `/proc/<missing>/cmdline` open fails with ENOENT.
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_read_cmdline_for_nonexistent_pid_returns_io_error() {
        // sysctl with a missing pid returns ESRCH; we surface it
        // verbatim as the os_error code. Don't pin the exact errno
        // because newer xnu builds occasionally remap it to EINVAL
        // for hardened tasks; just assert an os_error came through.
        let err = read_process_cmdline(0x7FFF_FFFE).expect_err("nonexistent pid");
        assert!(
            err.raw_os_error().is_some(),
            "expected an OS-level errno, got: {err}"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn read_cmdline_for_unknown_pid_returns_io_error() {
        // PID well above the typical Windows range â€” the OpenProcess
        // should fail with INVALID_PARAMETER or NOT_FOUND, which we
        // forward as the OS-level io::Error.
        let err = read_process_cmdline(0x7FFF_FFFE).expect_err("nonexistent pid");
        assert!(
            err.raw_os_error().is_some(),
            "expected an OS-level error code, got: {err}"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn read_cmdline_round_trips_known_args_from_spawned_child() {
        use crate::observer::ObserverConfig;
        use crate::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};
        use std::time::Duration;

        // Spawn a long-lived child with a distinctive argv, read its
        // cmdline back via NtQueryInformationProcess while it's still
        // alive, and assert the readback contains our argv markers.
        // `ping 127.0.0.1 -n 30` sleeps ~30s â€” plenty of time for the
        // readback before the child exits and is reaped.
        let cfg = ProcessConfig {
            command: CommandSpec::Argv(vec![
                "ping".into(),
                "127.0.0.1".into(),
                "-n".into(),
                "30".into(),
            ]),
            cwd: None,
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            address_space_limit_bytes: None,
        };
        let (process, _sub) = NativeProcess::with_observer(cfg, ObserverConfig::lifecycle());
        process.start().expect("spawn ping");
        let pid = process.pid().expect("pid");
        // Brief grace period so the process's PEB ProcessParameters is
        // fully initialized before we query.
        std::thread::sleep(Duration::from_millis(150));

        let cmdline = read_process_cmdline(pid).expect("read cmdline");
        process.kill().ok();
        process.close().ok();

        // Match relevant tokens â€” Windows command-line argv quoting
        // and capitalization can vary, so just check substrings.
        assert!(
            cmdline.to_lowercase().contains("ping"),
            "expected 'ping' in cmdline, got: {cmdline:?}"
        );
        assert!(
            cmdline.contains("127.0.0.1"),
            "expected target IP in cmdline, got: {cmdline:?}"
        );
        assert!(
            cmdline.contains("30"),
            "expected -n count in cmdline, got: {cmdline:?}"
        );
    }
}
