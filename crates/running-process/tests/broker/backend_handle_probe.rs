#![cfg(feature = "client")]

use running_process::broker::backend_handle::BackendHandle;

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
