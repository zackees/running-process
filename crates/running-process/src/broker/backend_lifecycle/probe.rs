//! Endpoint and process identity checks for backend handles.

use crate::broker::backend_lifecycle::identity::DaemonProcess;
use crate::broker::backend_lifecycle::verify_pid::{self, ProcessHandle, VerifyPidError};
use crate::broker::protocol::Endpoint;

/// Verify that an endpoint refers to the expected daemon process.
pub fn probe_endpoint(
    endpoint: &Endpoint,
    expected: &DaemonProcess,
) -> Result<ProcessHandle, ProbeError> {
    if !same_endpoint(endpoint, &expected.ipc_endpoint) {
        return Err(ProbeError::EndpointMismatch);
    }
    verify_pid::verify_daemon_process(expected).map_err(ProbeError::VerifyPid)
}

/// Compare two endpoint identities exactly.
pub fn same_endpoint(left: &Endpoint, right: &Endpoint) -> bool {
    left.namespace_id == right.namespace_id && left.path == right.path
}

/// Errors returned while probing a backend endpoint.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// The caller-provided endpoint did not match the expected daemon endpoint.
    #[error("endpoint does not match expected daemon identity")]
    EndpointMismatch,
    /// The daemon process identity could not be verified.
    #[error(transparent)]
    VerifyPid(#[from] VerifyPidError),
}
