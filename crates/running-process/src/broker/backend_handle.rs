//! Public handle for a verified backend daemon.

use std::io;
use std::time::{Duration, Instant};

use crate::broker::backend_lifecycle::identity::IdentityError;
use crate::broker::backend_lifecycle::probe::{self, ProbeError};
use crate::broker::backend_lifecycle::verify_pid::{self, ProcessHandle, VerifyPidError};
use crate::broker::protocol::{CacheManifest, Endpoint};

pub use crate::broker::backend_lifecycle::DaemonProcess;

/// Result type returned by backend-handle operations.
pub type Result<T> = std::result::Result<T, BackendHandleError>;

/// A handle to a running backend daemon.
///
/// The handle carries the daemon identity needed to defend against stale
/// manifests and PID recycling before consumers connect to the IPC endpoint.
pub struct BackendHandle {
    /// Logical service name from the manifest or direct probe caller.
    pub service_name: String,
    /// Service version from the manifest or direct probe caller.
    pub service_version: String,
    /// Verified daemon process identity.
    pub daemon_process: DaemonProcess,
    #[cfg(unix)]
    pub(crate) pid_handle: Option<ProcessHandle>,
    #[cfg(windows)]
    pub(crate) process_handle: Option<ProcessHandle>,
}

impl BackendHandle {
    /// Connect to an existing backend by endpoint and verify process identity.
    ///
    /// This first-pass probe verifies the endpoint identity tuple, current boot
    /// ID, process liveness, and executable SHA-256. It returns `None` for stale
    /// manifests, dead PIDs, or mismatched daemon binaries.
    pub fn probe(endpoint: &Endpoint, expected: &DaemonProcess) -> Option<Self> {
        Self::probe_with_service("", "", endpoint, expected).ok()
    }

    /// Probe an existing backend and attach service metadata to the handle.
    pub fn probe_with_service(
        service_name: impl Into<String>,
        service_version: impl Into<String>,
        endpoint: &Endpoint,
        expected: &DaemonProcess,
    ) -> Result<Self> {
        let process_handle = probe::probe_endpoint(endpoint, expected)?;
        Ok(Self::from_verified(
            service_name.into(),
            service_version.into(),
            expected.clone(),
            process_handle,
        ))
    }

    /// Probe the `current_daemon` recorded in a cache manifest.
    pub fn probe_manifest(manifest: &CacheManifest) -> Option<Self> {
        Self::try_from_manifest(manifest).ok().flatten()
    }

    /// Fallible variant of [`Self::probe_manifest`] that preserves parse errors.
    pub fn try_from_manifest(manifest: &CacheManifest) -> Result<Option<Self>> {
        let Some(daemon_process) = DaemonProcess::from_manifest_current_daemon(manifest)? else {
            return Ok(None);
        };
        let handle = Self::probe_with_service(
            manifest.service_name.clone(),
            manifest.service_version.clone(),
            &daemon_process.ipc_endpoint,
            &daemon_process,
        )?;
        Ok(Some(handle))
    }

    /// Check liveness without opening a new IPC connection.
    pub fn is_alive(&self) -> bool {
        self.platform_handle()
            .map(|handle| handle.is_alive())
            .unwrap_or_else(|| verify_pid::process_is_alive(self.daemon_process.pid))
    }

    /// Open a fresh IPC connection to this backend.
    pub async fn connect(&self) -> Result<Connection> {
        Connection::connect(&self.daemon_process.ipc_endpoint).map_err(BackendHandleError::Connect)
    }

    /// Send a graceful shutdown signal and wait until the process exits.
    ///
    /// On Windows this foundation returns `GracefulTerminateUnsupported` until
    /// the broker shutdown request protocol lands.
    pub async fn shutdown(self, timeout: Duration) -> Result<()> {
        verify_pid::signal_terminate(self.daemon_process.pid)?;
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !self.is_alive() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(BackendHandleError::ShutdownTimeout {
            pid: self.daemon_process.pid,
        })
    }

    /// Force-kill the daemon process.
    pub fn force_kill(self) -> Result<()> {
        verify_pid::force_kill_pid(self.daemon_process.pid)?;
        Ok(())
    }

    fn from_verified(
        service_name: String,
        service_version: String,
        daemon_process: DaemonProcess,
        process_handle: ProcessHandle,
    ) -> Self {
        #[cfg(unix)]
        {
            Self {
                service_name,
                service_version,
                daemon_process,
                pid_handle: Some(process_handle),
            }
        }

        #[cfg(windows)]
        {
            Self {
                service_name,
                service_version,
                daemon_process,
                process_handle: Some(process_handle),
            }
        }
    }

    fn platform_handle(&self) -> Option<&ProcessHandle> {
        #[cfg(unix)]
        {
            self.pid_handle.as_ref()
        }

        #[cfg(windows)]
        {
            self.process_handle.as_ref()
        }
    }
}

/// A fresh IPC connection to a verified backend daemon.
pub struct Connection {
    stream: interprocess::local_socket::Stream,
}

impl Connection {
    /// Connect to a backend endpoint using the platform local-socket name type.
    pub fn connect(endpoint: &Endpoint) -> io::Result<Self> {
        if endpoint.path.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "backend endpoint path is empty",
            ));
        }
        let name = endpoint_name(&endpoint.path)?;

        use interprocess::local_socket::traits::Stream as _;
        let stream = interprocess::local_socket::Stream::connect(name)?;
        Ok(Self { stream })
    }

    /// Return the underlying `interprocess` stream.
    pub fn into_inner(self) -> interprocess::local_socket::Stream {
        self.stream
    }
}

/// Errors returned by `BackendHandle`.
#[derive(Debug, thiserror::Error)]
pub enum BackendHandleError {
    /// Daemon identity normalization failed.
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// Endpoint/process probing failed.
    #[error(transparent)]
    Probe(#[from] ProbeError),
    /// Opening an IPC connection failed.
    #[error("backend IPC connection failed: {0}")]
    Connect(io::Error),
    /// Process verification or signalling failed.
    #[error(transparent)]
    VerifyPid(#[from] VerifyPidError),
    /// Graceful shutdown timed out.
    #[error("backend shutdown timed out for pid {pid}")]
    ShutdownTimeout {
        /// Process ID that did not exit before the timeout.
        pid: u32,
    },
}

fn endpoint_name(path: &str) -> io::Result<interprocess::local_socket::Name<'_>> {
    use interprocess::local_socket::prelude::*;

    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        path.to_fs_name::<GenericFilePath>()
    }

    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        path.to_ns_name::<GenericNamespaced>()
    }
}
