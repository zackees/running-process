//! Backend launch abstraction for Hello registry misses.
//!
//! The router owns admission control and registry insertion. Launchers own the
//! platform-specific act of starting or discovering a backend and returning a
//! verified [`BackendHandle`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::broker::backend_handle::{BackendHandle, BackendHandleError, DaemonProcess};
use crate::broker::backend_lifecycle::identity::{sha256_file, IdentityError};
use crate::broker::host_identity;
use crate::broker::lifecycle::sid::{user_sid_hash, SidError};
use crate::broker::protocol::ServiceDefinition;
use crate::spawn_daemon;

use super::backend_endpoint_allocator::{BackendEndpointAllocator, BackendEndpointAllocatorError};
use super::backend_registry::BackendKey;

/// Environment variable containing the logical service name for a launched
/// backend.
pub const BACKEND_ENV_SERVICE_NAME: &str = "RUNNING_PROCESS_BROKER_V1_SERVICE_NAME";
/// Environment variable containing the negotiated service version.
pub const BACKEND_ENV_SERVICE_VERSION: &str = "RUNNING_PROCESS_BROKER_V1_SERVICE_VERSION";
/// Environment variable containing the backend IPC endpoint path.
pub const BACKEND_ENV_ENDPOINT_PATH: &str = "RUNNING_PROCESS_BROKER_V1_BACKEND_PIPE";
/// Environment variable containing the backend endpoint namespace.
pub const BACKEND_ENV_ENDPOINT_NAMESPACE: &str = "RUNNING_PROCESS_BROKER_V1_BACKEND_NAMESPACE";
/// Environment variable containing the broker instance id.
pub const BACKEND_ENV_INSTANCE: &str = "RUNNING_PROCESS_BROKER_V1_INSTANCE";

/// Inputs supplied to a backend launcher after Hello validation and budget
/// admission.
pub struct BackendLaunchRequest<'a> {
    /// Backend key being launched.
    pub key: &'a BackendKey,
    /// Service definition that authorized the requested backend.
    pub service_definition: &'a ServiceDefinition,
}

/// Launches or discovers one backend and returns a verified handle.
pub trait BackendLauncher: Send + Sync {
    /// Launch the requested backend.
    fn launch(
        &self,
        request: &BackendLaunchRequest<'_>,
    ) -> Result<BackendHandle, BackendLaunchError>;
}

/// Command-based backend launcher.
///
/// This launcher allocates the canonical v1 backend endpoint, starts
/// `ServiceDefinition.binary_path` as a detached daemon, passes the selected
/// endpoint through environment variables, and verifies the spawned process
/// identity before returning a [`BackendHandle`].
#[derive(Debug)]
pub struct CommandBackendLauncher {
    user_sid_hash: String,
    allocators: Mutex<HashMap<String, BackendEndpointAllocator>>,
    idle_timeout_secs: Option<u32>,
}

impl CommandBackendLauncher {
    /// Build a launcher for the current user.
    pub fn for_current_user() -> Result<Self, SidError> {
        Ok(Self::new(user_sid_hash()?))
    }

    /// Build a launcher with an explicit 16-hex user SID hash.
    pub fn new(user_sid_hash: impl Into<String>) -> Self {
        Self {
            user_sid_hash: user_sid_hash.into(),
            allocators: Mutex::new(HashMap::new()),
            idle_timeout_secs: Some(30),
        }
    }

    /// Override the idle timeout recorded in the verified daemon identity.
    pub fn with_idle_timeout_secs(mut self, idle_timeout_secs: Option<u32>) -> Self {
        self.idle_timeout_secs = idle_timeout_secs;
        self
    }

    fn allocate_endpoint(
        &self,
        request: &BackendLaunchRequest<'_>,
    ) -> Result<crate::broker::protocol::Endpoint, BackendLaunchError> {
        let namespace_id = request.key.instance.id();
        let mut allocators = self
            .allocators
            .lock()
            .map_err(|_| BackendLaunchError::AllocatorPoisoned)?;
        let allocator = allocators
            .entry(namespace_id.clone())
            .or_insert_with(|| BackendEndpointAllocator::new(&self.user_sid_hash, namespace_id));
        Ok(allocator.allocate()?)
    }
}

impl BackendLauncher for CommandBackendLauncher {
    fn launch(
        &self,
        request: &BackendLaunchRequest<'_>,
    ) -> Result<BackendHandle, BackendLaunchError> {
        let endpoint = self.allocate_endpoint(request)?;
        let binary_path = canonical_backend_binary(request.service_definition)?;
        let mut command = Command::new(&binary_path);
        command
            .env(BACKEND_ENV_SERVICE_NAME, &request.key.service_name)
            .env(BACKEND_ENV_SERVICE_VERSION, &request.key.service_version)
            .env(BACKEND_ENV_ENDPOINT_PATH, &endpoint.path)
            .env(BACKEND_ENV_ENDPOINT_NAMESPACE, &endpoint.namespace_id)
            .env(BACKEND_ENV_INSTANCE, request.key.instance.id());

        let mut child = spawn_daemon(&mut command).map_err(BackendLaunchError::Spawn)?;
        let daemon = daemon_identity_for_spawned_process(
            child.id(),
            binary_path,
            endpoint.clone(),
            self.idle_timeout_secs,
        )?;

        match BackendHandle::probe_with_service(
            request.key.service_name.clone(),
            request.key.service_version.clone(),
            &endpoint,
            &daemon,
        ) {
            Ok(handle) => Ok(handle),
            Err(err) => {
                let _ = child.kill();
                Err(BackendLaunchError::BackendHandle(err))
            }
        }
    }
}

/// Errors raised while launching a backend.
#[derive(Debug, thiserror::Error)]
pub enum BackendLaunchError {
    /// The service definition did not include a backend binary path.
    #[error("backend binary_path is empty")]
    EmptyBinaryPath,
    /// The service definition did not include the per-version allow-list root.
    #[error("backend per_version_binary_dir is empty")]
    EmptyPerVersionBinaryDir,
    /// The backend binary path could not be canonicalized.
    #[error("backend binary_path {path:?} could not be canonicalized: {source}")]
    CanonicalizeBinary {
        /// Path that failed canonicalization.
        path: PathBuf,
        /// Filesystem error.
        source: std::io::Error,
    },
    /// The backend allow-list root could not be canonicalized.
    #[error("backend per_version_binary_dir {path:?} could not be canonicalized: {source}")]
    CanonicalizeBinaryRoot {
        /// Root path that failed canonicalization.
        path: PathBuf,
        /// Filesystem error.
        source: std::io::Error,
    },
    /// The binary was outside the configured per-version allow-list root.
    #[error("backend binary {binary:?} is outside per-version root {root:?}")]
    BinaryOutsideAllowRoot {
        /// Canonical backend binary path.
        binary: PathBuf,
        /// Canonical allow-list root.
        root: PathBuf,
    },
    /// Endpoint allocator state was poisoned.
    #[error("backend endpoint allocator state was poisoned")]
    AllocatorPoisoned,
    /// Canonical endpoint allocation failed.
    #[error(transparent)]
    Endpoint(#[from] BackendEndpointAllocatorError),
    /// Detached process creation failed.
    #[error("backend daemon spawn failed: {0}")]
    Spawn(std::io::Error),
    /// Spawned daemon identity construction failed.
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// Spawned daemon verification failed.
    #[error(transparent)]
    BackendHandle(#[from] BackendHandleError),
    /// Test or custom launcher failure.
    #[error("{0}")]
    Launcher(String),
}

fn canonical_backend_binary(
    service_definition: &ServiceDefinition,
) -> Result<PathBuf, BackendLaunchError> {
    if service_definition.binary_path.is_empty() {
        return Err(BackendLaunchError::EmptyBinaryPath);
    }
    if service_definition.per_version_binary_dir.is_empty() {
        return Err(BackendLaunchError::EmptyPerVersionBinaryDir);
    }

    let binary = PathBuf::from(&service_definition.binary_path);
    let binary = std::fs::canonicalize(&binary).map_err(|source| {
        BackendLaunchError::CanonicalizeBinary {
            path: binary,
            source,
        }
    })?;

    let root = PathBuf::from(&service_definition.per_version_binary_dir);
    let root = std::fs::canonicalize(&root)
        .map_err(|source| BackendLaunchError::CanonicalizeBinaryRoot { path: root, source })?;

    if !binary.starts_with(&root) {
        return Err(BackendLaunchError::BinaryOutsideAllowRoot { binary, root });
    }

    Ok(binary)
}

fn daemon_identity_for_spawned_process(
    pid: u32,
    exe_path: PathBuf,
    ipc_endpoint: crate::broker::protocol::Endpoint,
    idle_timeout_secs: Option<u32>,
) -> Result<DaemonProcess, IdentityError> {
    let exe_sha256 = sha256_file(&exe_path)?;
    Ok(DaemonProcess {
        pid,
        exe_path: exe_path.clone(),
        exe_sha256,
        boot_id: host_identity::current_for_path(&exe_path).boot_id,
        ipc_endpoint,
        started_at_unix_ms: unix_now_ms(),
        idle_timeout_secs,
    })
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
