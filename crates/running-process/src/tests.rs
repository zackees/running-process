use super::*;

// ── StreamKind tests ──

#[test]
fn stream_kind_as_str_stdout() {
    assert_eq!(StreamKind::Stdout.as_str(), "stdout");
}

#[test]
fn stream_kind_as_str_stderr() {
    assert_eq!(StreamKind::Stderr.as_str(), "stderr");
}

#[test]
fn stream_kind_equality() {
    assert_eq!(StreamKind::Stdout, StreamKind::Stdout);
    assert_ne!(StreamKind::Stdout, StreamKind::Stderr);
}

// ── StreamEvent tests ──

#[test]
fn stream_event_clone() {
    let event = StreamEvent {
        stream: StreamKind::Stdout,
        line: b"hello".to_vec(),
    };
    let cloned = event.clone();
    assert_eq!(event, cloned);
}

// ── ReadStatus tests ──

#[test]
fn read_status_line_variant() {
    let status: ReadStatus<Vec<u8>> = ReadStatus::Line(b"data".to_vec());
    assert!(matches!(status, ReadStatus::Line(ref v) if v == b"data"));
}

#[test]
fn read_status_timeout_variant() {
    let status: ReadStatus<Vec<u8>> = ReadStatus::Timeout;
    assert!(matches!(status, ReadStatus::Timeout));
}

#[test]
fn read_status_eof_variant() {
    let status: ReadStatus<Vec<u8>> = ReadStatus::Eof;
    assert!(matches!(status, ReadStatus::Eof));
}

// ── ProcessError tests ──

#[test]
fn process_error_display_already_started() {
    assert_eq!(
        ProcessError::AlreadyStarted.to_string(),
        "process already started"
    );
}

#[test]
fn process_error_display_not_running() {
    assert_eq!(
        ProcessError::NotRunning.to_string(),
        "process is not running"
    );
}

#[test]
fn process_error_display_stdin_unavailable() {
    assert_eq!(
        ProcessError::StdinUnavailable.to_string(),
        "process stdin is not available"
    );
}

#[test]
fn process_error_display_timeout() {
    assert_eq!(ProcessError::Timeout.to_string(), "process timed out");
}

#[test]
fn process_error_display_spawn() {
    let err = ProcessError::Spawn(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "not found",
    ));
    assert!(err.to_string().contains("not found"));
}

#[test]
fn process_error_display_io() {
    let err = ProcessError::Io(std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "broken",
    ));
    assert!(err.to_string().contains("broken"));
}

// ── CommandSpec tests ──

#[test]
fn command_spec_shell_variant() {
    let spec = CommandSpec::Shell("echo hello".to_string());
    assert!(matches!(spec, CommandSpec::Shell(ref s) if s == "echo hello"));
}

#[test]
fn command_spec_argv_variant() {
    let spec = CommandSpec::Argv(vec!["echo".to_string(), "hello".to_string()]);
    assert!(matches!(spec, CommandSpec::Argv(ref v) if v.len() == 2));
}

// ── StdinMode / StderrMode tests ──

#[test]
fn stdin_mode_equality() {
    assert_eq!(StdinMode::Inherit, StdinMode::Inherit);
    assert_ne!(StdinMode::Piped, StdinMode::Null);
}

#[test]
fn stderr_mode_equality() {
    assert_eq!(StderrMode::Stdout, StderrMode::Stdout);
    assert_ne!(StderrMode::Stdout, StderrMode::Pipe);
}

// ── SharedState tests ──

#[test]
fn shared_state_new_with_capture() {
    let state = SharedState::new(true);
    let queues = state.queues.lock().unwrap();
    assert!(!queues.stdout_closed);
    assert!(!queues.stderr_closed);
    assert!(queues.stdout_queue.is_empty());
    assert!(queues.stderr_queue.is_empty());
}

#[test]
fn shared_state_new_without_capture() {
    let state = SharedState::new(false);
    let queues = state.queues.lock().unwrap();
    assert!(queues.stdout_closed);
    assert!(queues.stderr_closed);
}

#[test]
fn shared_state_returncode_initially_not_set() {
    let state = SharedState::new(true);
    let code = state.returncode.load(Ordering::Acquire);
    assert_eq!(code, RETURNCODE_NOT_SET);
}

// ── feed_chunk tests ──

#[test]
fn feed_chunk_single_line_with_newline() {
    let shared = Arc::new(SharedState::new(true));
    let mut pending = Vec::new();
    let lines = feed_chunk(&mut pending, b"hello\n");
    emit_lines(&shared, StreamKind::Stdout, lines);
    let queues = shared.queues.lock().unwrap();
    assert_eq!(queues.stdout_queue.len(), 1);
    assert_eq!(queues.stdout_queue[0], b"hello");
    assert!(pending.is_empty());
}

#[test]
fn feed_chunk_crlf_stripping() {
    let shared = Arc::new(SharedState::new(true));
    let mut pending = Vec::new();
    let lines = feed_chunk(&mut pending, b"hello\r\n");
    emit_lines(&shared, StreamKind::Stdout, lines);
    let queues = shared.queues.lock().unwrap();
    assert_eq!(queues.stdout_queue.len(), 1);
    assert_eq!(queues.stdout_queue[0], b"hello");
}

#[test]
fn feed_chunk_multiple_lines() {
    let shared = Arc::new(SharedState::new(true));
    let mut pending = Vec::new();
    let lines = feed_chunk(&mut pending, b"a\nb\nc\n");
    emit_lines(&shared, StreamKind::Stdout, lines);
    let queues = shared.queues.lock().unwrap();
    assert_eq!(queues.stdout_queue.len(), 3);
    assert_eq!(queues.stdout_queue[0], b"a");
    assert_eq!(queues.stdout_queue[1], b"b");
    assert_eq!(queues.stdout_queue[2], b"c");
}

#[test]
fn feed_chunk_no_newline_stays_pending() {
    let mut pending = Vec::new();
    let lines = feed_chunk(&mut pending, b"partial");
    assert!(lines.is_empty());
    assert_eq!(pending, b"partial");
}

#[test]
fn feed_chunk_accumulates_pending() {
    let shared = Arc::new(SharedState::new(true));
    let mut pending = Vec::new();
    let lines1 = feed_chunk(&mut pending, b"hel");
    emit_lines(&shared, StreamKind::Stdout, lines1);
    let lines2 = feed_chunk(&mut pending, b"lo\n");
    emit_lines(&shared, StreamKind::Stdout, lines2);
    let queues = shared.queues.lock().unwrap();
    assert_eq!(queues.stdout_queue.len(), 1);
    assert_eq!(queues.stdout_queue[0], b"hello");
    assert!(pending.is_empty());
}

#[test]
fn feed_chunk_empty_line_not_emitted() {
    let shared = Arc::new(SharedState::new(true));
    let mut pending = Vec::new();
    let lines = feed_chunk(&mut pending, b"\n");
    emit_lines(&shared, StreamKind::Stdout, lines);
    let queues = shared.queues.lock().unwrap();
    assert!(queues.stdout_queue.is_empty());
}

#[test]
fn feed_chunk_stderr_goes_to_stderr_queue() {
    let shared = Arc::new(SharedState::new(true));
    let mut pending = Vec::new();
    let lines = feed_chunk(&mut pending, b"error\n");
    emit_lines(&shared, StreamKind::Stderr, lines);
    let queues = shared.queues.lock().unwrap();
    assert!(queues.stdout_queue.is_empty());
    assert_eq!(queues.stderr_queue.len(), 1);
    assert_eq!(queues.stderr_queue[0], b"error");
}

// ── emit_lines tests ──

#[test]
fn emit_lines_updates_all_queues_and_history() {
    let shared = Arc::new(SharedState::new(true));
    emit_lines(&shared, StreamKind::Stdout, vec![b"test".to_vec()]);
    let queues = shared.queues.lock().unwrap();
    assert_eq!(queues.stdout_queue.len(), 1);
    assert_eq!(queues.stdout_history.len(), 1);
    assert_eq!(queues.stdout_history_bytes, 4);
    assert_eq!(queues.combined_queue.len(), 1);
    assert_eq!(queues.combined_history.len(), 1);
    assert_eq!(queues.combined_history_bytes, 4);
}

#[test]
fn emit_lines_stderr_updates_stderr_queues() {
    let shared = Arc::new(SharedState::new(true));
    emit_lines(&shared, StreamKind::Stderr, vec![b"err".to_vec()]);
    let queues = shared.queues.lock().unwrap();
    assert_eq!(queues.stderr_queue.len(), 1);
    assert_eq!(queues.stderr_history.len(), 1);
    assert_eq!(queues.stderr_history_bytes, 3);
    assert_eq!(queues.combined_queue.len(), 1);
    assert_eq!(queues.combined_history_bytes, 3);
}

// ── NativeProcess unit tests (no process spawn) ──

#[test]
fn native_process_returncode_none_before_start() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec!["echo".into()]),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });
    assert!(process.returncode().is_none());
}

#[test]
fn native_process_pid_none_before_start() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec!["echo".into()]),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });
    assert!(process.pid().is_none());
}

#[test]
fn native_process_has_pending_false_when_no_capture() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec!["echo".into()]),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });
    assert!(!process.has_pending_stream(StreamKind::Stdout));
    assert!(!process.has_pending_combined());
}

#[test]
fn native_process_drain_empty_when_no_capture() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec!["echo".into()]),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });
    assert!(process.drain_stream(StreamKind::Stdout).is_empty());
    assert!(process.drain_combined().is_empty());
}

#[test]
fn native_process_stderr_not_pending_when_merged() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec!["echo".into()]),
        cwd: None,
        env: None,
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });
    assert!(!process.has_pending_stream(StreamKind::Stderr));
}

#[test]
fn native_process_drain_stderr_empty_when_merged() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec!["echo".into()]),
        cwd: None,
        env: None,
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });
    assert!(process.drain_stream(StreamKind::Stderr).is_empty());
}

#[test]
fn native_process_captured_stderr_empty_when_merged() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec!["echo".into()]),
        cwd: None,
        env: None,
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });
    assert!(process.captured_stderr().is_empty());
}

#[test]
fn native_process_captured_stream_bytes_zero_when_merged_stderr() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec!["echo".into()]),
        cwd: None,
        env: None,
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });
    assert_eq!(process.captured_stream_bytes(StreamKind::Stderr), 0);
}

#[test]
fn native_process_clear_captured_stderr_zero_when_merged() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec!["echo".into()]),
        cwd: None,
        env: None,
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });
    assert_eq!(process.clear_captured_stream(StreamKind::Stderr), 0);
}

#[test]
fn native_process_read_stream_eof_when_stderr_merged() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec!["echo".into()]),
        cwd: None,
        env: None,
        capture: true,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });
    assert_eq!(
        process.read_stream(StreamKind::Stderr, Some(Duration::from_millis(10))),
        ReadStatus::Eof
    );
}

// ── log_spawned_child_pid ──

#[test]
fn log_spawned_child_pid_noop_without_env() {
    std::env::remove_var("RUNNING_PROCESS_CHILD_PID_LOG_PATH");
    assert!(log_spawned_child_pid(12345).is_ok());
}

// ── shell_command ──

#[test]
fn shell_command_creates_command() {
    let cmd = shell_command("echo test");
    let _ = format!("{:?}", cmd);
}

// ── exit_code ──

#[test]
fn exit_code_from_success() {
    let output = std::process::Command::new("python")
        .args(["-c", "pass"])
        .output()
        .unwrap();
    assert_eq!(exit_code(output.status), 0);
}

#[test]
fn exit_code_from_nonzero() {
    let output = std::process::Command::new("python")
        .args(["-c", "import sys; sys.exit(42)"])
        .output()
        .unwrap();
    assert_eq!(exit_code(output.status), 42);
}

// ── windows_priority_flags ──

#[cfg(windows)]
mod windows_tests {
    use super::*;
    use crate::windows::windows_priority_flags;

    const IDLE_PRIORITY_CLASS: u32 = 0x0000_0040;
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
    const ABOVE_NORMAL_PRIORITY_CLASS: u32 = 0x0000_8000;
    const HIGH_PRIORITY_CLASS: u32 = 0x0000_0080;

    #[test]
    fn priority_flags_none() {
        assert_eq!(windows_priority_flags(None), 0);
    }

    #[test]
    fn priority_flags_zero() {
        assert_eq!(windows_priority_flags(Some(0)), 0);
    }

    #[test]
    fn priority_flags_high_nice_idle() {
        assert_eq!(windows_priority_flags(Some(15)), IDLE_PRIORITY_CLASS);
        assert_eq!(windows_priority_flags(Some(20)), IDLE_PRIORITY_CLASS);
    }

    #[test]
    fn priority_flags_low_positive_below_normal() {
        assert_eq!(windows_priority_flags(Some(1)), BELOW_NORMAL_PRIORITY_CLASS);
        assert_eq!(
            windows_priority_flags(Some(14)),
            BELOW_NORMAL_PRIORITY_CLASS
        );
    }

    #[test]
    fn priority_flags_negative_above_normal() {
        assert_eq!(
            windows_priority_flags(Some(-1)),
            ABOVE_NORMAL_PRIORITY_CLASS
        );
        assert_eq!(
            windows_priority_flags(Some(-14)),
            ABOVE_NORMAL_PRIORITY_CLASS
        );
    }

    #[test]
    fn priority_flags_very_negative_high() {
        assert_eq!(windows_priority_flags(Some(-15)), HIGH_PRIORITY_CLASS);
        assert_eq!(windows_priority_flags(Some(-20)), HIGH_PRIORITY_CLASS);
    }

    // ── windows_creation_flags (#584, console-gated by #622) ──

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    #[test]
    fn creation_flags_default_hides_console_for_consoleless_parent() {
        // A plain child of a console-less parent (the daemon) gets
        // CREATE_NO_WINDOW by default — the #584 flash fix.
        let flags = windows_creation_flags(None, false, None, false);
        assert_ne!(flags & CREATE_NO_WINDOW, 0, "default must hide the console");
    }

    #[test]
    fn creation_flags_console_parent_keeps_shared_console() {
        // #622 regression: a console-attached parent's child must inherit
        // the shared console (no injected CREATE_NO_WINDOW), or CTRL_C
        // delivery via GenerateConsoleCtrlEvent breaks.
        let flags = windows_creation_flags(None, false, None, true);
        assert_eq!(
            flags & CREATE_NO_WINDOW,
            0,
            "console-attached parent must not detach its child"
        );
    }

    #[test]
    fn creation_flags_console_parent_keeps_group_and_priority() {
        // The #622 gate must only drop the hide bit — group + priority
        // still apply for console-attached parents.
        let flags = windows_creation_flags(None, true, Some(15), true);
        assert_eq!(flags & CREATE_NO_WINDOW, 0);
        assert_ne!(flags & CREATE_NEW_PROCESS_GROUP, 0);
        assert_ne!(flags & windows_priority_flags(Some(15)), 0);
    }

    #[test]
    fn creation_flags_new_console_opts_out() {
        // A caller asking for a visible console is respected — no injected
        // CREATE_NO_WINDOW.
        let flags = windows_creation_flags(Some(CREATE_NEW_CONSOLE), false, None, false);
        assert_eq!(flags & CREATE_NO_WINDOW, 0);
        assert_ne!(flags & CREATE_NEW_CONSOLE, 0);
    }

    #[test]
    fn creation_flags_detached_process_opts_out() {
        // DETACHED_PROCESS is the caller's own "no console" choice; don't
        // also OR in CREATE_NO_WINDOW (the two are mutually exclusive to
        // CreateProcessW).
        let flags = windows_creation_flags(Some(DETACHED_PROCESS), false, None, false);
        assert_eq!(flags & CREATE_NO_WINDOW, 0);
        assert_ne!(flags & DETACHED_PROCESS, 0);
    }

    #[test]
    fn creation_flags_explicit_no_window_not_doubled() {
        let flags = windows_creation_flags(Some(CREATE_NO_WINDOW), false, None, false);
        assert_eq!(flags, CREATE_NO_WINDOW);
    }

    #[test]
    fn creation_flags_explicit_no_window_wins_over_console_parent() {
        // A caller explicitly asking to hide the console is honoured even
        // when the parent has one.
        let flags = windows_creation_flags(Some(CREATE_NO_WINDOW), false, None, true);
        assert_eq!(flags, CREATE_NO_WINDOW);
    }

    #[test]
    fn creation_flags_preserves_process_group_and_priority() {
        // Group + priority bits are OR-ed in alongside the default hide.
        let flags = windows_creation_flags(None, true, Some(15), false);
        assert_ne!(flags & CREATE_NO_WINDOW, 0);
        assert_ne!(flags & CREATE_NEW_PROCESS_GROUP, 0);
        assert_ne!(flags & windows_priority_flags(Some(15)), 0);
    }

    #[test]
    fn creation_flags_group_survives_console_opt_out() {
        // Opting out of the hidden default must not drop the process group.
        let flags = windows_creation_flags(Some(CREATE_NEW_CONSOLE), true, None, false);
        assert_eq!(flags & CREATE_NO_WINDOW, 0);
        assert_ne!(flags & CREATE_NEW_PROCESS_GROUP, 0);
        assert_ne!(flags & CREATE_NEW_CONSOLE, 0);
    }
}

// ── ProcessConfig ──

#[test]
fn process_config_clone() {
    let config = ProcessConfig {
        command: CommandSpec::Shell("echo".to_string()),
        cwd: Some("/tmp".into()),
        env: Some(vec![("KEY".to_string(), "VAL".to_string())]),
        capture: true,
        stderr_mode: StderrMode::Pipe,
        creationflags: Some(0x10),
        create_process_group: true,
        stdin_mode: StdinMode::Piped,
        nice: Some(5),
    };
    let cloned = config.clone();
    assert!(cloned.capture);
    assert_eq!(cloned.nice, Some(5));
}

// ── render_rust_debug_traces ──

#[test]
fn render_rust_debug_traces_returns_string() {
    let result = render_rust_debug_traces();
    let _ = result.len();
}

// ── RustDebugScopeGuard ──

#[test]
fn rust_debug_scope_guard_enters_and_drops() {
    let _guard = RustDebugScopeGuard::enter("test_scope", file!(), line!());
    let traces = render_rust_debug_traces();
    assert!(traces.contains("test_scope"));
    drop(_guard);
}

// ── Unix signal tests ──

#[cfg(unix)]
mod unix_tests {
    use super::*;
    use crate::unix::unix_signal_raw;

    #[test]
    fn unix_signal_raw_values() {
        assert_eq!(unix_signal_raw(UnixSignal::Interrupt), libc::SIGINT);
        assert_eq!(unix_signal_raw(UnixSignal::Terminate), libc::SIGTERM);
        assert_eq!(unix_signal_raw(UnixSignal::Kill), libc::SIGKILL);
    }

    #[test]
    fn unix_signal_enum_equality() {
        assert_eq!(UnixSignal::Interrupt, UnixSignal::Interrupt);
        assert_ne!(UnixSignal::Interrupt, UnixSignal::Kill);
    }
}

// ── wait_for_capture_completion ──

#[test]
fn wait_for_capture_completion_noop_without_capture() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec!["echo".into()]),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });
    process.wait_for_capture_completion_impl();
}

// ── build_command tests ──

#[test]
fn build_command_from_argv() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec!["echo".into(), "hello".into(), "world".into()]),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });
    let cmd = process.build_command();
    assert_eq!(cmd.get_program(), "echo");
    let args: Vec<_> = cmd.get_args().collect();
    assert_eq!(args, vec!["hello", "world"]);
}

#[test]
fn build_command_from_shell() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Shell("echo test".into()),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });
    let cmd = process.build_command();
    // Shell commands go through the OS shell
    let program = cmd.get_program().to_string_lossy().to_string();
    #[cfg(windows)]
    assert!(
        program.contains("cmd"),
        "expected cmd shell, got {}",
        program
    );
    #[cfg(not(windows))]
    assert!(program.contains("sh"), "expected sh shell, got {}", program);
}

#[test]
fn build_command_with_cwd() {
    let tmp = std::env::temp_dir();
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec!["echo".into()]),
        cwd: Some(tmp.clone()),
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });
    let cmd = process.build_command();
    assert_eq!(cmd.get_current_dir().unwrap(), &tmp);
}

#[test]
fn build_command_with_env() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec!["echo".into()]),
        cwd: None,
        env: Some(vec![
            ("FOO".into(), "bar".into()),
            ("BAZ".into(), "qux".into()),
        ]),
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });
    let cmd = process.build_command();
    let envs: Vec<_> = cmd.get_envs().collect();
    assert!(envs
        .iter()
        .any(|(k, v)| *k == "FOO" && *v == Some(std::ffi::OsStr::new("bar"))));
    assert!(envs
        .iter()
        .any(|(k, v)| *k == "BAZ" && *v == Some(std::ffi::OsStr::new("qux"))));
}

#[test]
fn build_command_single_argv() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec!["echo".into()]),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });
    let cmd = process.build_command();
    assert_eq!(cmd.get_program(), "echo");
    assert_eq!(cmd.get_args().count(), 0);
}

// ── set_returncode tests ──

#[test]
fn set_returncode_updates_shared_state() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec!["echo".into()]),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });
    assert!(process.returncode().is_none());
    process.set_returncode(42);
    assert_eq!(process.returncode(), Some(42));
}

#[test]
fn set_returncode_overwrites() {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec!["echo".into()]),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    });
    process.set_returncode(1);
    process.set_returncode(2);
    assert_eq!(process.returncode(), Some(2));
}

// ── SharedState with capture ──

#[test]
fn shared_state_with_capture_queues_open() {
    let state = SharedState::new(true);
    let guard = state.queues.lock().unwrap();
    assert!(!guard.stdout_closed);
    assert!(!guard.stderr_closed);
}

#[test]
fn shared_state_without_capture_queues_closed() {
    let state = SharedState::new(false);
    let guard = state.queues.lock().unwrap();
    assert!(guard.stdout_closed);
    assert!(guard.stderr_closed);
}

// ── ProcessError Display additional variants ──

#[test]
fn process_error_display_io_variant() {
    let err = ProcessError::Io(std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "pipe broken",
    ));
    let msg = format!("{}", err);
    assert!(msg.contains("pipe broken"));
}

#[test]
fn process_error_display_spawn_variant() {
    let err = ProcessError::Spawn(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "not found",
    ));
    let msg = format!("{}", err);
    assert!(msg.contains("not found"));
}

// ── shell_command produces a command ──

#[test]
fn shell_command_returns_command_with_shell() {
    let cmd = shell_command("echo test");
    let program = cmd.get_program().to_string_lossy().to_string();
    #[cfg(windows)]
    assert!(program.contains("cmd"));
    #[cfg(not(windows))]
    assert!(program.contains("sh"));
}
