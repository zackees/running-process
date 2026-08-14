//! Portable facade for native command-line inspection.

pub fn read_process_cmdline(pid: u32) -> std::io::Result<String> {
    running_process_platform_internal::platform::process::read_process_cmdline(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observer::ObserverConfig;
    use crate::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};
    use std::time::Duration;

    fn fixture_program() -> String {
        let exe = std::env::current_exe().expect("test executable path");
        let dir = exe
            .parent()
            .and_then(std::path::Path::parent)
            .expect("test binary should live in <profile>/deps/");
        dir.join(format!(
            "testbin-stdio-scripted{}",
            std::env::consts::EXE_SUFFIX
        ))
        .to_string_lossy()
        .into_owned()
    }

    fn config(args: Vec<String>) -> ProcessConfig {
        ProcessConfig {
            command: CommandSpec::Argv(args),
            cwd: None,
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
            address_space_limit_bytes: None,
        }
    }

    #[test]
    fn read_cmdline_for_pid_zero_returns_invalid_input() {
        let err = read_process_cmdline(0).expect_err("pid 0 should be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn read_cmdline_round_trips_known_args_from_spawned_child() {
        let marker = "running-process-cmdline-marker";
        let (process, _sub) = NativeProcess::with_observer(
            config(vec![
                fixture_program(),
                "sleep-ms:30000".into(),
                format!("out:{marker}"),
            ]),
            ObserverConfig::lifecycle(),
        );
        process.start().expect("spawn fixture");
        let pid = process.pid().expect("pid");
        std::thread::sleep(Duration::from_millis(150));

        let cmdline = read_process_cmdline(pid).expect("read cmdline");
        process.kill().ok();
        process.close().ok();
        assert!(
            cmdline.contains("testbin-stdio-scripted"),
            "expected fixture name in cmdline, got: {cmdline:?}"
        );
        assert!(
            cmdline.contains(marker),
            "expected marker in cmdline, got: {cmdline:?}"
        );
    }

    #[test]
    fn read_cmdline_for_unknown_pid_returns_an_os_error() {
        let err = read_process_cmdline(0x7FFF_FFFE).expect_err("nonexistent pid");
        assert!(
            err.raw_os_error().is_some() || err.kind() == std::io::ErrorKind::NotFound,
            "expected an OS-level missing-process error, got: {err}"
        );
    }
}
