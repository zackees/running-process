#![cfg(feature = "client")]

use running_process::broker::backend_handle::{BackendHandle, BackendHandleError};
use running_process::broker::backend_lifecycle::probe::ProbeError;
use running_process::broker::backend_lifecycle::verify_pid::VerifyPidError;

use crate::backend_handle_common::current_daemon;

#[test]
fn probe_current_process_returns_live_handle() {
    let daemon = current_daemon();
    let handle =
        BackendHandle::probe_with_service("zccache", "1.2.3", &daemon.ipc_endpoint, &daemon)
            .unwrap();

    assert_eq!(handle.service_name, "zccache");
    assert_eq!(handle.service_version, "1.2.3");
    assert_eq!(handle.daemon_process.pid, std::process::id());
    assert!(handle.is_alive());
}

#[test]
fn probe_rejects_executable_path_mismatch_even_when_hash_matches() {
    let mut daemon = current_daemon();
    let fake_exe = std::env::temp_dir().join(format!(
        "running-process-backend-handle-copy-{}{}",
        std::process::id(),
        std::env::consts::EXE_SUFFIX
    ));
    let _ = std::fs::remove_file(&fake_exe);
    std::fs::copy(&daemon.exe_path, &fake_exe).unwrap();

    daemon.exe_path = fake_exe.clone();
    let result =
        BackendHandle::probe_with_service("zccache", "1.2.3", &daemon.ipc_endpoint, &daemon);
    let _ = std::fs::remove_file(&fake_exe);

    assert!(matches!(
        result,
        Err(BackendHandleError::Probe(ProbeError::VerifyPid(
            VerifyPidError::ExePathMismatch { .. }
        )))
    ));
}
