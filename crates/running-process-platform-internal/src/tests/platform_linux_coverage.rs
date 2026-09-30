use super::*;
use crate::platform::process::{
    CaptureStream, NonInvasiveObservationGrade, ObserverCategory, ObserverScope, ObserverSupport,
    ProcessCommandConfig, UnixSignalKind,
};
use std::ffi::OsStr;
use std::io::Write as _;
#[cfg(feature = "async-process")]
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn capability_environment_signals_and_observer_matrix_are_complete() {
    let pairs = vec![("A".into(), "1".into()), ("A".into(), "2".into())];
    assert_eq!(canonical_environment_pairs(pairs.clone()), pairs);
    assert!(monitor_console_windows(Duration::ZERO).is_empty());
    assert!(process_snapshot().is_empty());
    assert!(process_snapshot_for_pid(std::process::id()).is_none());
    assert!(!parent_has_console());

    let capability = exact_trace_capability();
    assert!(capability.available);
    assert_eq!(capability.backend, "linux-ptrace");
    assert_eq!(
        capability.non_invasive_grade,
        NonInvasiveObservationGrade::SnapshotInferred
    );

    let interrupt = unix_signal_raw(UnixSignalKind::Interrupt);
    let terminate = unix_signal_raw(UnixSignalKind::Terminate);
    let kill = unix_signal_raw(UnixSignalKind::Kill);
    assert!(interrupt > 0 && terminate > 0 && kill > 0);
    assert_ne!(interrupt, terminate);
    assert_ne!(terminate, kill);

    for (scope, category, support, backend) in [
        (
            ObserverScope::SystemWide,
            ObserverCategory::File,
            ObserverSupport::Unavailable,
            "seccomp-user-notify",
        ),
        (
            ObserverScope::SystemWide,
            ObserverCategory::Network,
            ObserverSupport::Unavailable,
            "ebpf",
        ),
        (
            ObserverScope::SystemWide,
            ObserverCategory::Process,
            ObserverSupport::Unavailable,
            "seccomp-user-notify",
        ),
        (
            ObserverScope::LaunchedProcessTree,
            ObserverCategory::File,
            ObserverSupport::Partial,
            "proc-fd-snapshot",
        ),
        (
            ObserverScope::LaunchedProcessTree,
            ObserverCategory::Network,
            ObserverSupport::Unavailable,
            "none",
        ),
        (
            ObserverScope::LaunchedProcessTree,
            ObserverCategory::Process,
            ObserverSupport::Supported,
            "subreaper-proc-poll",
        ),
    ] {
        let result = observer_backend(scope, category);
        assert!(std::mem::discriminant(&result.support) == std::mem::discriminant(&support));
        assert_eq!(result.backend, backend);
        assert!(!result.reason.is_empty());
    }

    let absent = i32::MAX as u32;
    assert!(soft_terminate_process_group(absent).is_ok());
    assert!(unix_signal_process(absent, UnixSignalKind::Kill).is_err());
    assert!(unix_signal_process_group(i32::MAX, UnixSignalKind::Terminate).is_err());
    assert!(unix_set_priority(absent, 0).is_err());
}

#[test]
fn gnu_note_parser_accepts_build_ids_and_rejects_malformed_notes() {
    let mut note = Vec::new();
    note.extend_from_slice(&4_u32.to_ne_bytes());
    note.extend_from_slice(&3_u32.to_ne_bytes());
    note.extend_from_slice(&3_u32.to_ne_bytes());
    note.extend_from_slice(b"GNU\0");
    note.extend_from_slice(&[1, 2, 3, 0]);
    assert_eq!(gnu_build_id_from_notes(&note), Some(&[1, 2, 3][..]));

    let mut other = note.clone();
    other[8..12].copy_from_slice(&1_u32.to_ne_bytes());
    assert_eq!(gnu_build_id_from_notes(&other), None);
    assert_eq!(gnu_build_id_from_notes(&note[..note.len() - 1]), None);
    assert_eq!(gnu_build_id_from_notes(&[]), None);
}

#[cfg(feature = "async-process")]
#[test]
fn exit_status_and_shell_spec_preserve_linux_conventions() {
    let terminate = unix_signal_raw(UnixSignalKind::Terminate);
    let exited = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 7"])
        .status()
        .unwrap();
    let signaled = std::process::Command::new("/bin/sh")
        .args(["-c", "kill -TERM $$"])
        .status()
        .unwrap();
    assert_eq!(exit_code(exited), 7);
    assert_eq!(trampoline_exit_code(exited), 7);
    assert_eq!(exit_code(signaled), -terminate);
    assert_eq!(trampoline_exit_code(signaled), 128 + terminate);

    let spec = shell_spec(OsStr::new("printf coverage"));
    assert_eq!(spec.program, OsStr::new("/bin/sh"));
    assert_eq!(spec.args, [OsStr::new("-c"), OsStr::new("printf coverage")]);
}

#[test]
fn linux_only_stubs_and_shell_builders_are_explicit() {
    let command_text = "printf platform-coverage";
    for command in [shell_command(command_text), compat_shell_command(command_text)] {
        assert_eq!(command.get_program(), OsStr::new("/bin/sh"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [OsStr::new("-lc"), OsStr::new(command_text)]
        );
    }

    let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
    let error = match assign_child_to_windows_job(&child, child.id(), None, None) {
        Ok(_) => panic!("Linux cannot create a Windows Job Object"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert_eq!(sync_child_native_handle(&child), 0);
    assert!(child.wait().unwrap().success());
}

#[test]
fn capture_readers_deliver_data_and_wake_on_cancellation() {
    let cancellation = Arc::new(CaptureCancellation::default());
    let (mut writer, reader) = UnixStream::pair().unwrap();
    let mut prepared =
        prepare_capture_reader(reader, &cancellation, CaptureStream::Stdout).unwrap();
    assert_eq!(prepared.read(&mut []).unwrap(), 0);
    writer.write_all(b"output").unwrap();
    let mut bytes = [0_u8; 6];
    prepared.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"output");
    capture_reader_done(&cancellation, CaptureStream::Stdout);

    let (_writer, reader) = UnixStream::pair().unwrap();
    let mut blocked =
        prepare_capture_reader(reader, &cancellation, CaptureStream::Stderr).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let mut byte = [0_u8; 1];
        tx.send(blocked.read(&mut byte).unwrap_err().kind()).unwrap();
    });
    cancel_capture_reader(&cancellation);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        io::ErrorKind::Interrupted
    );
    reader_thread.join().unwrap();
    capture_reader_done(&cancellation, CaptureStream::Stderr);
    assert!(set_nonblocking(-1).is_err());
}

#[cfg(feature = "async-process")]
#[test]
fn reviewed_command_configuration_executes_on_short_lived_children() {
    set_process_name("running-process-platform-name-is-truncated");

    let mut plain = std::process::Command::new("/bin/true");
    configure_trampoline_command(&mut plain);
    configure_process_command(&mut plain, ProcessCommandConfig::default()).unwrap();
    assert!(plain.status().unwrap().success());

    let mut configured = std::process::Command::new("/bin/true");
    configure_process_command(
        &mut configured,
        ProcessCommandConfig {
            create_process_group: true,
            // 19 can only lower (or preserve) inherited priority, so this
            // branch never requires CAP_SYS_NICE on a pre-reniced runner.
            nice: Some(19),
            address_space_limit_bytes: Some(512 * 1024 * 1024),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(configured.status().unwrap().success());

    let mut daemon = std::process::Command::new("/bin/true");
    configure_sync_daemon_command(&mut daemon).unwrap();
    assert!(daemon.status().unwrap().success());

    let mut contained = std::process::Command::new("/bin/true");
    configure_sync_contained_command(&mut contained).unwrap();
    assert!(contained.status().unwrap().success());

    let mut tokio_command = Command::new("/bin/true");
    configure_compat_tokio_command(&mut tokio_command, false, false).unwrap();
    configure_command(&mut tokio_command, true, true, None).unwrap();
}

#[cfg(feature = "async-process")]
#[test]
fn owner_death_configuration_composes_with_group_priority_and_limits() {
    // Some Nix-based development hosts intentionally lack `/bin/true`; use
    // the shell-resolved utility here because this test exercises pre-exec
    // policy rather than a particular filesystem spelling.
    let mut configured = std::process::Command::new("true");
    configure_process_command_for_bounded_owner_death(
        &mut configured,
        ProcessCommandConfig {
            create_process_group: true,
            // This can only lower (or preserve) inherited priority, so it
            // remains valid without CAP_SYS_NICE.
            nice: Some(19),
            address_space_limit_bytes: Some(512 * 1024 * 1024),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(configured.status().unwrap().success());
}

#[cfg(feature = "async-process")]
#[test]
fn tokio_configuration_and_live_signal_helpers_reach_the_os() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut command = Command::new("/bin/sleep");
        command
            .arg("30")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        configure_compat_tokio_command(&mut command, false, true).unwrap();
        let mut child = command.spawn().unwrap();
        after_compat_tokio_spawn(&child, true)
            .expect("containment must be reported, not assumed");
        unix_signal_process(child.id().unwrap(), UnixSignalKind::Terminate).unwrap();
        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .expect("signalled child did not exit within cleanup deadline")
            .unwrap();
        assert!(!status.success());

        let mut grouped = Command::new("/bin/sleep");
        grouped.arg("30").kill_on_drop(true);
        configure_command(&mut grouped, true, false, None).unwrap();
        let mut child = grouped.spawn().unwrap();
        after_spawn(&child, false, None).expect("a no-op must still succeed");
        let pid = child.id().unwrap();
        unix_signal_process_group(pid as i32, UnixSignalKind::Terminate).unwrap();
        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .expect("signalled process group did not exit within cleanup deadline")
            .unwrap();
        assert!(!status.success());
    });
}

#[cfg(feature = "async-process")]
#[test]
fn nice_alone_is_applied_after_spawn_and_owner_death_keeps_pre_exec() {
    // Owner-death runs in the child, so it keeps `pre_exec` and carries nice
    // with it; nice alone must not (#1248).
    assert_eq!(nice_after_spawn(false, Some(7)), Some(7));
    assert_eq!(nice_after_spawn(false, None), None);
    assert_eq!(nice_after_spawn(true, Some(7)), None);
    assert_eq!(nice_after_spawn(true, None), None);
}

#[cfg(feature = "async-process")]
#[test]
fn nice_only_spawn_reports_the_requested_niceness_once_spawn_completes() {
    // /proc/<pid>/stat field 19 is the niceness of the child's main thread.
    fn niceness(pid: u32) -> i32 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let tail = &stat[stat.rfind(')').unwrap() + 1..];
        tail.split_ascii_whitespace().nth(16).unwrap().parse().unwrap()
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        // Raising the value only lowers priority, so no CAP_SYS_NICE is needed.
        let requested = niceness(std::process::id()).max(0) + 3;
        let mut command = Command::new("/bin/sleep");
        command.arg("30").kill_on_drop(true);
        configure_command(&mut command, false, false, Some(requested)).unwrap();
        let mut child = command.spawn().unwrap();
        after_spawn(&child, false, Some(requested)).unwrap();
        let pid = child.id().unwrap();
        assert_eq!(niceness(pid), requested);
        child.kill().await.unwrap();
    });
}

#[cfg(feature = "async-process")]
#[test]
fn nice_only_spawn_kills_the_child_when_the_priority_cannot_be_applied() {
    // Lowering niceness below the inherited value needs CAP_SYS_NICE.
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let effective_uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|fields| fields.split_ascii_whitespace().nth(1))
        .and_then(|uid| uid.parse::<u32>().ok())
        .unwrap();
    if effective_uid == 0 {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut command = Command::new("/bin/sleep");
        command.arg("30").kill_on_drop(true);
        configure_command(&mut command, false, false, Some(-10)).unwrap();
        let mut child = command.spawn().unwrap();
        assert!(after_spawn(&child, false, Some(-10)).is_err());
        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .expect("a child that missed its priority must not keep running")
            .unwrap();
        assert!(!status.success());
    });
}

#[test]
fn fault_code_names_are_byte_exact() {
    // #974 PR 2: moved out of probe-daemon's crash store, spelling unchanged.
    assert_eq!(process_fault_code_name(11), "SIGSEGV");
    assert_eq!(process_fault_code_name(7), "SIGBUS");
    assert_eq!(process_fault_code_name(4), "SIGILL");
    assert_eq!(process_fault_code_name(8), "SIGFPE");
    assert_eq!(process_fault_code_name(6), "SIGABRT");
    assert_eq!(process_fault_code_name(5), "SIGTRAP");
    assert_eq!(process_fault_code_name(99), "signal-99");
}

#[cfg(feature = "ipc")]
#[test]
fn component_runtime_dirs_are_byte_exact_for_the_probe() {
    // #974 PR 2: probe-daemon's discovery directory used to be derived in the
    // daemon. Pin the spelling so a running daemon's discovery file stays
    // findable across an upgrade, and pin that sockets live inside it.
    let xdg = Some(std::ffi::OsString::from("/run/user/1000"));
    assert_eq!(
        component_runtime_dir_in(xdg.clone(), 1000, "probe"),
        std::path::PathBuf::from("/run/user/1000/running-process/probe")
    );
    assert_eq!(
        component_runtime_dir_in(None, 1000, "probe"),
        std::path::PathBuf::from("/tmp/running-process-1000/probe")
    );
    let socket = component_endpoint_path_in(xdg.clone(), 1000, "probe", "x");
    assert_eq!(
        std::path::Path::new(&socket).parent(),
        Some(component_runtime_dir_in(xdg, 1000, "probe").as_path())
    );
}

#[cfg(feature = "ipc")]
#[test]
fn component_endpoint_paths_are_byte_exact_for_the_probe_and_the_broker() {
    // #974: probe-daemon used to derive these itself. Pinning the spelling
    // keeps sockets that a running daemon already published reachable.
    let xdg = Some(std::ffi::OsString::from("/run/user/1000"));
    assert_eq!(
        component_endpoint_path_in(xdg.clone(), 1000, "probe", "rpp-probe-abc-0"),
        "/run/user/1000/running-process/probe/rpp-probe-abc-0.sock"
    );
    assert_eq!(
        component_endpoint_path_in(None, 1000, "probe", "rpp-probe-abc-0"),
        "/tmp/running-process-1000/probe/rpp-probe-abc-0.sock"
    );
    // The broker now goes through the same primitive; its spelling must not move.
    assert_eq!(
        component_endpoint_path_in(xdg, 1000, "broker-v2", "rpb-v2-x-0"),
        "/run/user/1000/running-process/broker-v2/rpb-v2-x-0.sock"
    );
    assert_eq!(
        component_endpoint_path_in(None, 1000, "broker-v2", "rpb-v2-x-0"),
        "/tmp/running-process-1000/broker-v2/rpb-v2-x-0.sock"
    );
}

#[cfg(feature = "ipc")]
#[test]
fn probe_and_broker_never_share_a_socket_directory() {
    let probe = component_endpoint_path_in(None, 7, "probe", "same-name-0");
    let broker = component_endpoint_path_in(None, 7, "broker-v2", "same-name-0");
    assert_ne!(probe, broker);
    assert_eq!(
        std::path::Path::new(&probe).file_name(),
        std::path::Path::new(&broker).file_name()
    );
}

#[cfg(feature = "ipc")]
#[test]
fn broker_endpoint_name_is_the_broker_component_path() {
    assert_eq!(
        ipc_broker_endpoint_name("rpb-v2-x-0", false).unwrap(),
        ipc_component_endpoint_path("broker-v2", "rpb-v2-x-0")
    );
}

#[cfg(feature = "async-process")]
#[tokio::test]
async fn command_override_keeps_argv_and_applies_the_native_limit_mapping() {
    let raw = OsStr::new("arg with 'quotes' and spaces");
    let mut command = std::process::Command::new("/bin/sh");
    command
        .arg("-c")
        .arg("printf '%s|' \"$1\"; ulimit -v")
        .arg("sh")
        .arg(raw);
    let output = crate::SpawnSpec::from_std_command(command)
        .address_space_limit_bytes(Some(1 << 40))
        .stdin(crate::StreamMode::Null)
        .stdout(crate::StreamMode::Piped)
        .stderr(crate::StreamMode::Piped)
        .spawn()
        .await
        .expect("spawn override")
        .wait_with_output()
        .await
        .expect("wait override");
    assert!(output.status.success(), "{output:?}");
    let mut expected = b"arg with 'quotes' and spaces|".to_vec();
    expected.extend_from_slice(format!("{}\n", (1u64 << 40) / 1024).as_bytes());
    assert_eq!(output.stdout, expected);
}
