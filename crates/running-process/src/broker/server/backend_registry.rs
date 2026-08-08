//! Verified backend registry keyed by broker instance, service, and version.

use std::collections::HashMap;

use crate::broker::backend_handle::BackendHandle;
use crate::broker::protocol::ServiceDefinition;
use crate::broker::server::hello_handler::RegisteredBackend;
use crate::broker::server::instance::BrokerInstanceKey;

/// Lookup key for one backend process.
///
/// The key includes the daemon executable's content hash (`exe_sha256`, hex)
/// so that two *different builds of the same version* — the ordinary
/// edit-rebuild-the-daemon dev loop — are distinct registry entries rather
/// than aliasing to one. Without it, a rebuilt daemon binary negotiates to the
/// resident (stale-code) daemon on a `service_name` + `service_version` match,
/// the un-isolated half of the daemon-collision class (running-process#894).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BackendKey {
    /// Broker trust-domain instance.
    pub instance: BrokerInstanceKey,
    /// Logical service name.
    pub service_name: String,
    /// Service version.
    pub service_version: String,
    /// SHA-256 (lowercase hex) of the daemon executable this backend runs.
    ///
    /// Derived on `insert` from the launched daemon's verified identity, and
    /// supplied on lookup as the hash of the on-disk `binary_path` the client
    /// would launch. A rebuild changes the bytes → changes this segment → the
    /// resident daemon is a lookup miss and the caller launches its own.
    pub exe_sha256: String,
}

impl BackendKey {
    /// Build a key from an instance, service tuple, and daemon exe hash.
    pub fn new(
        instance: BrokerInstanceKey,
        service_name: impl Into<String>,
        service_version: impl Into<String>,
        exe_sha256: impl Into<String>,
    ) -> Self {
        Self {
            instance,
            service_name: service_name.into(),
            service_version: service_version.into(),
            exe_sha256: exe_sha256.into(),
        }
    }
}

/// In-memory table of verified backend handles.
#[derive(Default)]
pub struct BackendRegistry {
    entries: HashMap<BackendKey, BackendHandle>,
}

impl BackendRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Number of registered backend handles.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return true when the registry has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Insert or replace one verified backend handle.
    ///
    /// The key's `exe_sha256` segment is taken from the handle's verified
    /// daemon identity, so a backend is always registered under the content
    /// hash of the binary it actually launched.
    pub fn insert(
        &mut self,
        instance: BrokerInstanceKey,
        handle: BackendHandle,
    ) -> Option<BackendHandle> {
        let key = BackendKey::new(
            instance,
            handle.service_name.clone(),
            handle.service_version.clone(),
            hex_lower(&handle.daemon_process.exe_sha256),
        );
        self.entries.insert(key, handle)
    }

    /// Return one handle by exact instance/service/version/exe-hash key.
    ///
    /// `exe_sha256` is the lowercase-hex content hash of the daemon binary the
    /// caller intends to reach. A handle registered under a different hash
    /// (i.e. an earlier build of the same version) does not match.
    pub fn get(
        &self,
        instance: &BrokerInstanceKey,
        service_name: &str,
        service_version: &str,
        exe_sha256: &str,
    ) -> Option<&BackendHandle> {
        self.entries.get(&BackendKey::new(
            instance.clone(),
            service_name,
            service_version,
            exe_sha256,
        ))
    }

    /// Return one handle by instance/service/version, ignoring the exe hash.
    ///
    /// For callers that are *re-locating a backend they already negotiated*
    /// (the single-backend direct-serve path, or a Windows handoff for a
    /// connection whose Hello already picked a backend) rather than making a
    /// fresh routing decision. Routing decisions must use [`Self::get`], which
    /// is hash-exact, so a rebuilt daemon does not alias the resident one.
    /// If more than one build is registered, the first match is returned.
    pub fn get_any_build(
        &self,
        instance: &BrokerInstanceKey,
        service_name: &str,
        service_version: &str,
    ) -> Option<&BackendHandle> {
        self.entries.iter().find_map(|(key, handle)| {
            (key.instance == *instance
                && key.service_name == service_name
                && key.service_version == service_version)
                .then_some(handle)
        })
    }

    /// Iterate over all registered backend handles.
    pub fn iter(&self) -> impl Iterator<Item = (&BackendKey, &BackendHandle)> {
        self.entries.iter()
    }

    /// Remove backend handles whose verified process is no longer alive.
    ///
    /// Returns the removed keys so the lifecycle monitor can emit events,
    /// metrics, or diagnostics after the registry mutation is complete.
    pub fn prune_stale(&mut self) -> Vec<BackendKey> {
        let mut removed = Vec::new();
        self.entries.retain(|key, handle| {
            let alive = handle.is_alive();
            if !alive {
                removed.push(key.clone());
            }
            alive
        });
        removed
    }

    /// Return Hello negotiation metadata for one registered backend.
    ///
    /// `expected_exe_sha256` is the lowercase-hex content hash of the on-disk
    /// daemon binary the client would launch. A resident daemon of the same
    /// service+version but a *different* build hash is not returned, so the
    /// caller falls through to launching its own (running-process#894).
    pub fn registered_backend_for(
        &self,
        instance: &BrokerInstanceKey,
        service_definition: &ServiceDefinition,
        service_version: &str,
        expected_exe_sha256: &str,
    ) -> Option<RegisteredBackend> {
        let handle = self.get(
            instance,
            &service_definition.service_name,
            service_version,
            expected_exe_sha256,
        )?;
        Some(RegisteredBackend {
            service_definition: service_definition.clone(),
            daemon_version: handle.service_version.clone(),
            backend_pipe: handle.daemon_process.ipc_endpoint.path.clone(),
            server_capabilities: 0,
        })
    }

    /// Like [`Self::registered_backend_for`] but hash-agnostic — for the
    /// single-backend direct-serve path that fronts exactly one build.
    pub fn registered_backend_for_any_build(
        &self,
        instance: &BrokerInstanceKey,
        service_definition: &ServiceDefinition,
        service_version: &str,
    ) -> Option<RegisteredBackend> {
        let handle = self.get_any_build(instance, &service_definition.service_name, service_version)?;
        Some(RegisteredBackend {
            service_definition: service_definition.clone(),
            daemon_version: handle.service_version.clone(),
            backend_pipe: handle.daemon_process.ipc_endpoint.path.clone(),
            server_capabilities: 0,
        })
    }
}

/// Lowercase-hex encoding of a 32-byte digest, for use as a `BackendKey`
/// segment. Kept local so the registry key has no external hex dependency.
pub(crate) fn hex_lower(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(64);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::broker::backend_handle::{BackendHandle, DaemonProcess};
    use crate::broker::protocol::Endpoint;

    use super::*;

    fn handle(service_name: &str, version: &str, pid: u32) -> BackendHandle {
        let endpoint = Endpoint {
            namespace_id: "shared".into(),
            path: format!("rpb-v1-test-{service_name}-{version}"),
        };
        let mut daemon = DaemonProcess::current_process(endpoint, Some(30)).unwrap();
        daemon.pid = pid;

        BackendHandle {
            service_name: service_name.into(),
            service_version: version.into(),
            daemon_process: daemon,
            #[cfg(unix)]
            pid_handle: None,
            #[cfg(windows)]
            process_handle: None,
        }
    }

    /// The exe hash every `handle()` in this module carries: the test binary's
    /// own content hash (all handles are `DaemonProcess::current_process`).
    fn test_exe_hash() -> String {
        hex_lower(&handle("probe", "0.0.0", std::process::id()).daemon_process.exe_sha256)
    }

    #[test]
    fn prune_stale_removes_dead_handles_and_keeps_live_ones() {
        let mut registry = BackendRegistry::new();
        let exe = test_exe_hash();
        let live_key = BackendKey::new(BrokerInstanceKey::Shared, "zccache", "1.11.20", &exe);
        let dead_key = BackendKey::new(BrokerInstanceKey::Shared, "zccache", "1.11.21", &exe);

        registry.insert(
            live_key.instance.clone(),
            handle(
                &live_key.service_name,
                &live_key.service_version,
                std::process::id(),
            ),
        );
        registry.insert(
            dead_key.instance.clone(),
            handle(&dead_key.service_name, &dead_key.service_version, u32::MAX),
        );

        let removed = registry.prune_stale();

        assert_eq!(removed, vec![dead_key.clone()]);
        assert!(registry
            .get(
                &live_key.instance,
                &live_key.service_name,
                &live_key.service_version,
                &exe,
            )
            .is_some());
        assert!(registry
            .get(
                &dead_key.instance,
                &dead_key.service_name,
                &dead_key.service_version,
                &exe,
            )
            .is_none());
    }

    #[test]
    fn same_version_different_build_is_a_distinct_entry() {
        // Two daemons, same service+version, different executable hash: the
        // core running-process#894 case (a dev rebuild of the daemon binary).
        let mut registry = BackendRegistry::new();

        let mut a = handle("zccache", "1.11.20", std::process::id());
        a.daemon_process.exe_sha256 = [0xAA; 32];
        let mut b = handle("zccache", "1.11.20", std::process::id());
        b.daemon_process.exe_sha256 = [0xBB; 32];
        let a_pipe = a.daemon_process.ipc_endpoint.path.clone();

        registry.insert(BrokerInstanceKey::Shared, a);
        // A different build of the SAME version must NOT overwrite build A.
        let replaced = registry.insert(BrokerInstanceKey::Shared, b);
        assert!(
            replaced.is_none(),
            "a different exe hash must be a new registry entry, not a replacement"
        );
        assert_eq!(registry.len(), 2, "both builds coexist");

        // A client that would launch build A reaches build A, never build B.
        let got = registry
            .get(
                &BrokerInstanceKey::Shared,
                "zccache",
                "1.11.20",
                &hex_lower(&[0xAA; 32]),
            )
            .expect("build A is reachable by its own hash");
        assert_eq!(got.daemon_process.ipc_endpoint.path, a_pipe);

        // A hash that matches neither build is a clean miss (→ caller launches).
        assert!(registry
            .get(
                &BrokerInstanceKey::Shared,
                "zccache",
                "1.11.20",
                &hex_lower(&[0xCC; 32]),
            )
            .is_none());
    }
}
