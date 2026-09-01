//! Normalized daemon identity carried by `BackendHandle`.
//!
//! `DaemonProcess` is the typed form of `CacheManifest.current_daemon`. It is
//! deliberately more specific than the generated protobuf message: paths are
//! `PathBuf`s, executable hashes are fixed 32-byte arrays, and the IPC endpoint
//! is required. That keeps malformed manifests out of the `BackendHandle` probe
//! path.

use std::convert::TryFrom;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::broker::host_identity;
use crate::broker::protocol::{self, CacheManifest, Endpoint};

/// A backend daemon identity with fixed-width fields suitable for verification.
///
/// This mirrors `CacheManifest.current_daemon`, but normalizes protobuf strings
/// and byte vectors into path and digest types that are harder to misuse.
///
/// Persist this value only after the daemon has selected its final IPC endpoint
/// and executable. Later consumers can pass the same identity to
/// [`crate::broker::backend_handle::BackendHandle::probe`] or store it as
/// `CacheManifest.current_daemon`.
///
/// ```no_run
/// use running_process::broker::backend_handle::DaemonProcess;
/// use running_process::broker::protocol::{CacheManifest, Endpoint};
///
/// # fn example(mut manifest: CacheManifest)
/// #     -> Result<CacheManifest, running_process::broker::backend_lifecycle::identity::IdentityError>
/// # {
/// let endpoint = Endpoint {
///     namespace_id: "host-namespace".to_owned(),
///     path: "running-process-backend.sock".to_owned(),
/// };
/// let daemon = DaemonProcess::current_process(endpoint, Some(600))?;
///
/// manifest.current_daemon = Some(daemon.to_proto());
/// # Ok(manifest)
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonProcess {
    /// Operating-system process ID.
    pub pid: u32,
    /// Executable path recorded when the daemon identity was written.
    pub exe_path: PathBuf,
    /// BLAKE3 content hash of the daemon executable.
    pub exe_hash: [u8; 32],
    /// SHA-256 digest retained on the v1 wire for pre-BLAKE3 stable brokers.
    pub legacy_exe_sha256: [u8; 32],
    /// Host boot ID observed when the daemon started.
    pub boot_id: String,
    /// IPC endpoint used to connect to the daemon.
    pub ipc_endpoint: Endpoint,
    /// Daemon start timestamp in Unix milliseconds.
    pub started_at_unix_ms: u64,
    /// Optional idle timeout advertised by the daemon.
    pub idle_timeout_secs: Option<u32>,
}

/// Hash material recorded when constructing a current daemon identity.
///
/// [`Self::LegacyCompatible`] preserves the historical default for consumers
/// that must authenticate to brokers released before the BLAKE3 migration.
/// [`Self::Blake3Only`] avoids a second full executable read when the
/// application has established that the legacy slot must remain all zeroes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DaemonIdentityHashPolicy {
    /// Compute BLAKE3 and the legacy SHA-256 compatibility digest.
    #[default]
    LegacyCompatible,
    /// Compute BLAKE3 only and encode the fixed-width legacy slot as zeroes.
    Blake3Only,
}

impl DaemonProcess {
    /// Build a daemon identity for the current process.
    ///
    /// This is primarily useful for tests and direct-daemon consumers that have
    /// just spawned a backend and need to persist a manifest entry.
    ///
    /// The executable digest is taken from `std::env::current_exe()` at the time
    /// this method runs. If a daemon relocates or replaces its executable after
    /// startup, record the final identity after relocation instead.
    pub fn current_process(
        ipc_endpoint: Endpoint,
        idle_timeout_secs: Option<u32>,
    ) -> Result<Self, IdentityError> {
        Self::current_process_with_hash_policy(
            ipc_endpoint,
            idle_timeout_secs,
            DaemonIdentityHashPolicy::LegacyCompatible,
        )
    }

    /// Build a current-process identity with an explicit legacy-digest policy.
    ///
    /// The default [`Self::current_process`] remains legacy compatible. This
    /// variant exists for a direct daemon whose stable contract fixes the
    /// historical SHA-256 probe field to zero and must avoid the extra file
    /// read that computing that digest would require.
    pub fn current_process_with_hash_policy(
        ipc_endpoint: Endpoint,
        idle_timeout_secs: Option<u32>,
        hash_policy: DaemonIdentityHashPolicy,
    ) -> Result<Self, IdentityError> {
        let exe_path = std::env::current_exe().map_err(IdentityError::CurrentExe)?;
        let exe_hash = executable_hash_file(&exe_path)?;
        let legacy_exe_sha256 = match hash_policy {
            DaemonIdentityHashPolicy::LegacyCompatible => sha256_file(&exe_path)?,
            DaemonIdentityHashPolicy::Blake3Only => [0; 32],
        };
        Ok(Self {
            pid: std::process::id(),
            exe_path,
            exe_hash,
            legacy_exe_sha256,
            boot_id: host_identity::current().boot_id,
            ipc_endpoint,
            started_at_unix_ms: unix_now_ms(),
            idle_timeout_secs,
        })
    }

    /// Convert this identity into the protobuf form stored in `CacheManifest`.
    ///
    /// The conversion preserves the fixed-width BLAKE3 value as bytes and
    /// names its algorithm explicitly on the wire.
    pub fn to_proto(&self) -> protocol::DaemonProcess {
        protocol::DaemonProcess {
            pid: self.pid,
            exe_path: self.exe_path.to_string_lossy().into_owned(),
            exe_hash_algorithm: EXECUTABLE_HASH_ALGORITHM.to_owned(),
            exe_hash: self.exe_hash.to_vec(),
            ipc_endpoint: Some(self.ipc_endpoint.clone()),
            started_at_unix_ms: self.started_at_unix_ms,
            boot_id: self.boot_id.clone(),
            idle_timeout_secs: self.idle_timeout_secs,
        }
    }

    /// Encode a daemon identity for an endpoint probe.
    ///
    /// Tag 3 remains reserved in the current protobuf schema, but stable
    /// pre-BLAKE3 brokers still decode it as the executable SHA-256.  Preserve
    /// their read path by appending that historical unknown field after the
    /// canonical BLAKE3 message. Current protobuf readers safely ignore it.
    pub fn encode_probe_identity(&self, output: &mut Vec<u8>) -> Result<(), prost::EncodeError> {
        use prost::Message;

        self.to_proto().encode(output)?;
        output.push(0x1a); // field 3, length-delimited
        output.push(32); // fixed digest length, encoded as a one-byte varint
        output.extend_from_slice(&self.legacy_exe_sha256);
        Ok(())
    }

    /// Read and normalize `CacheManifest.current_daemon`.
    ///
    /// Returns `Ok(None)` when the manifest has no daemon entry. Malformed
    /// entries, such as a missing endpoint or non-32-byte executable digest,
    /// return an [`IdentityError`].
    pub fn from_manifest_current_daemon(
        manifest: &CacheManifest,
    ) -> Result<Option<Self>, IdentityError> {
        manifest
            .current_daemon
            .clone()
            .map(Self::try_from)
            .transpose()
    }
}

impl TryFrom<protocol::DaemonProcess> for DaemonProcess {
    type Error = IdentityError;

    fn try_from(value: protocol::DaemonProcess) -> Result<Self, Self::Error> {
        let ipc_endpoint = value.ipc_endpoint.ok_or(IdentityError::MissingEndpoint)?;
        if value.exe_hash_algorithm != EXECUTABLE_HASH_ALGORITHM {
            return Err(IdentityError::UnsupportedExecutableHashAlgorithm(
                value.exe_hash_algorithm,
            ));
        }
        let exe_hash =
            vec_to_hash(value.exe_hash).map_err(IdentityError::InvalidExecutableHashLength)?;
        Ok(Self {
            pid: value.pid,
            exe_path: PathBuf::from(value.exe_path),
            exe_hash,
            // Decoded identities are never served as a newly launched daemon.
            // Keep the reserved compatibility payload local to identities we
            // create from a verified executable path.
            legacy_exe_sha256: [0; 32],
            boot_id: value.boot_id,
            ipc_endpoint,
            started_at_unix_ms: value.started_at_unix_ms,
            idle_timeout_secs: value.idle_timeout_secs,
        })
    }
}

impl From<&DaemonProcess> for protocol::DaemonProcess {
    fn from(value: &DaemonProcess) -> Self {
        value.to_proto()
    }
}

impl Serialize for DaemonProcess {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        DaemonProcessSerde::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DaemonProcess {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = DaemonProcessSerde::deserialize(deserializer)?;
        if value.exe_hash_algorithm != EXECUTABLE_HASH_ALGORITHM {
            return Err(<D::Error as serde::de::Error>::custom(format!(
                "unsupported daemon executable hash algorithm {:?}; expected blake3",
                value.exe_hash_algorithm
            )));
        }
        Ok(Self {
            pid: value.pid,
            exe_path: value.exe_path,
            exe_hash: value.exe_hash,
            legacy_exe_sha256: value.legacy_exe_sha256,
            boot_id: value.boot_id,
            ipc_endpoint: value.ipc_endpoint.into(),
            started_at_unix_ms: value.started_at_unix_ms,
            idle_timeout_secs: value.idle_timeout_secs,
        })
    }
}

/// Errors returned while normalizing daemon identity.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// The protobuf daemon identity did not include an IPC endpoint.
    #[error("daemon process is missing ipc_endpoint")]
    MissingEndpoint,
    /// The protobuf daemon identity used an unsupported or legacy hash contract.
    #[error("unsupported daemon executable hash algorithm {0:?}; expected blake3")]
    UnsupportedExecutableHashAlgorithm(String),
    /// The protobuf daemon identity had an executable digest with the wrong size.
    #[error("daemon process exe_hash must be 32 bytes, got {0}")]
    InvalidExecutableHashLength(usize),
    /// The current executable path could not be read.
    #[error("failed to resolve current executable: {0}")]
    CurrentExe(io::Error),
    /// A filesystem operation failed while hashing the executable.
    #[error("failed to hash executable: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DaemonProcessSerde {
    pid: u32,
    exe_path: PathBuf,
    exe_hash_algorithm: String,
    exe_hash: [u8; 32],
    #[serde(default)]
    legacy_exe_sha256: [u8; 32],
    boot_id: String,
    ipc_endpoint: EndpointSerde,
    started_at_unix_ms: u64,
    idle_timeout_secs: Option<u32>,
}

impl From<&DaemonProcess> for DaemonProcessSerde {
    fn from(value: &DaemonProcess) -> Self {
        Self {
            pid: value.pid,
            exe_path: value.exe_path.clone(),
            exe_hash_algorithm: EXECUTABLE_HASH_ALGORITHM.to_owned(),
            exe_hash: value.exe_hash,
            legacy_exe_sha256: value.legacy_exe_sha256,
            boot_id: value.boot_id.clone(),
            ipc_endpoint: EndpointSerde::from(&value.ipc_endpoint),
            started_at_unix_ms: value.started_at_unix_ms,
            idle_timeout_secs: value.idle_timeout_secs,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EndpointSerde {
    namespace_id: String,
    path: String,
}

impl From<&Endpoint> for EndpointSerde {
    fn from(value: &Endpoint) -> Self {
        Self {
            namespace_id: value.namespace_id.clone(),
            path: value.path.clone(),
        }
    }
}

impl From<EndpointSerde> for Endpoint {
    fn from(value: EndpointSerde) -> Self {
        Endpoint {
            namespace_id: value.namespace_id,
            path: value.path,
        }
    }
}

/// BLAKE3 content hash used by the daemon identity wire contract.
pub fn executable_hash_file(path: &Path) -> Result<[u8; 32], io::Error> {
    crate::content_hash::blake3_file(path).map(|hash| *hash.as_bytes())
}

/// SHA-256 helper retained for the independent process-probe wire contract.
pub fn sha256_file(path: &Path) -> Result<[u8; 32], io::Error> {
    let bytes = fs::read(path)?;
    let digest = Sha256::digest(&bytes);
    let mut out = [0_u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

fn vec_to_hash(bytes: Vec<u8>) -> Result<[u8; 32], usize> {
    let len = bytes.len();
    let Ok(out) = bytes.try_into() else {
        return Err(len);
    };
    Ok(out)
}

const EXECUTABLE_HASH_ALGORITHM: &str = "blake3";

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod broker_dance_identity_tests {
    //! The broker owns the daemon "dance" (relocation / identity / lifecycle),
    //! and it keys a backend on its `DaemonProcess` identity — whose distinctive
    //! field is `exe_hash`, the content hash of the (relocated) executable.
    //!
    //! These pin the invariant that lets the broker keep two *builds* apart
    //! instead of letting them displace each other as "stale-version" — the
    //! exact collision that spawn-stormed soldr's per-process self-managed
    //! daemon when the identity did NOT carry the hash (zackees/soldr#2352).
    use super::*;
    use prost::Message;

    /// Schema used by brokers released before the BLAKE3 identity migration.
    #[derive(Clone, PartialEq, Message)]
    struct LegacyDaemonProcess {
        #[prost(uint32, tag = "1")]
        pid: u32,
        #[prost(string, tag = "2")]
        exe_path: String,
        #[prost(bytes = "vec", tag = "3")]
        exe_sha256: Vec<u8>,
    }

    #[test]
    fn executable_identity_hash_uses_blake3() {
        let path =
            std::env::temp_dir().join(format!("running-process-946-hash-{}", std::process::id()));
        std::fs::write(&path, b"daemon image bytes").expect("write fixture");
        let actual = executable_hash_file(&path).expect("hash fixture");
        std::fs::remove_file(&path).ok();

        assert_eq!(actual, *blake3::hash(b"daemon image bytes").as_bytes());
    }

    #[test]
    fn blake3_identity_dual_writes_a_legacy_sha256_for_stable_brokers() {
        let identity = DaemonProcess::current_process(endpoint("compat.sock"), None)
            .expect("current daemon identity");
        let current = identity.to_proto();
        let mut encoded = Vec::new();
        identity
            .encode_probe_identity(&mut encoded)
            .expect("encode compatibility identity");
        let legacy = LegacyDaemonProcess::decode(encoded.as_slice())
            .expect("pre-blake3 broker decodes current identity");

        assert_eq!(current.exe_hash_algorithm, EXECUTABLE_HASH_ALGORITHM);
        assert_eq!(legacy.exe_sha256.len(), 32);
        assert_eq!(
            legacy.exe_sha256,
            sha256_file(&identity.exe_path)
                .expect("sha256 executable")
                .to_vec(),
            "the legacy wire field remains verifiable by a stable broker"
        );
    }

    #[test]
    fn blake3_only_identity_skips_the_legacy_sha256_pass_and_keeps_tag_three_zeroed() {
        let identity = DaemonProcess::current_process_with_hash_policy(
            endpoint("blake3-only.sock"),
            None,
            DaemonIdentityHashPolicy::Blake3Only,
        )
        .expect("current daemon identity");
        let mut encoded = Vec::new();
        identity
            .encode_probe_identity(&mut encoded)
            .expect("encode compatibility identity");
        let legacy = LegacyDaemonProcess::decode(encoded.as_slice())
            .expect("pre-blake3 broker decodes current identity");

        assert_eq!(identity.legacy_exe_sha256, [0; 32]);
        assert_eq!(legacy.exe_sha256, [0; 32]);
        assert_eq!(
            identity.exe_hash,
            executable_hash_file(&identity.exe_path).expect("blake3 executable")
        );
    }

    #[test]
    fn blake3_only_identity_sidecar_records_a_zero_legacy_digest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("identity.json");
        let identity = DaemonProcess::current_process_with_hash_policy(
            endpoint("blake3-only-sidecar.sock"),
            None,
            DaemonIdentityHashPolicy::Blake3Only,
        )
        .expect("current daemon identity");

        crate::broker::backend_sdk::write_daemon_identity_file(&path, &identity)
            .expect("write sidecar");
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read sidecar"))
                .expect("parse sidecar");

        let legacy = json
            .get("legacy_exe_sha256")
            .and_then(serde_json::Value::as_array)
            .expect("legacy digest array");
        assert_eq!(legacy.len(), 32);
        assert!(legacy.iter().all(|byte| byte == &serde_json::json!(0)));
    }

    fn endpoint(path: &str) -> Endpoint {
        Endpoint {
            namespace_id: "ns".to_owned(),
            path: path.to_owned(),
        }
    }

    fn identity(exe_hash: [u8; 32]) -> DaemonProcess {
        // Everything else is held constant so `exe_hash` is the only variable:
        // a distinct *build* of the same daemon differs only in its bytes.
        DaemonProcess {
            pid: 1234,
            exe_path: PathBuf::from("runtime/soldr-self/v0.8.44-deadbeef/soldr.exe"),
            exe_hash,
            legacy_exe_sha256: [0x24; 32],
            boot_id: "boot-1".to_owned(),
            ipc_endpoint: endpoint("rpb-v2-soldr-daemon-0123456789abcdef-0"),
            started_at_unix_ms: 1,
            idle_timeout_secs: Some(600),
        }
    }

    #[test]
    fn distinct_builds_get_distinct_identities() {
        let a = identity([0xAA; 32]);

        // One byte of the binary changed => a rebuild.
        let mut rebuilt = [0xAA; 32];
        rebuilt[0] = 0xBB;
        let b = identity(rebuilt);

        assert_ne!(
            a, b,
            "a different executable hash must produce a distinct daemon identity, \
             so the broker can never conflate two builds (no stale-version war)"
        );

        // Same bytes => same identity: a client and the same-build daemon it
        // spawns rendezvous on ONE identity, with no shared file or negotiation.
        assert_eq!(
            a,
            identity([0xAA; 32]),
            "identical executable bytes must yield the same identity"
        );
    }

    #[test]
    fn pre_4_10_4_json_defaults_the_missing_legacy_sha256() {
        let original = identity([0x42; 32]);
        let mut legacy_json = serde_json::to_value(&original).expect("serialize identity");
        let object = legacy_json.as_object_mut().expect("identity JSON object");
        assert!(object.remove("legacy_exe_sha256").is_some());

        let restored: DaemonProcess =
            serde_json::from_value(legacy_json).expect("read pre-4.10.4 identity JSON");
        let mut expected = original;
        expected.legacy_exe_sha256 = [0; 32];
        assert_eq!(restored, expected);
    }

    #[test]
    fn exe_sha256_survives_the_manifest_wire_round_trip() {
        // The broker distinguishes backends off the `CacheManifest` on the wire,
        // so the 32-byte content hash must round-trip intact — otherwise two
        // builds could collapse to one identity in transit.
        let original = identity([0x42; 32]);
        let proto = original.to_proto();
        assert_eq!(proto.exe_hash_algorithm, "blake3");
        assert_eq!(
            proto.exe_hash.len(),
            32,
            "the wire form must carry the full 32-byte BLAKE3 hash"
        );
        let restored = DaemonProcess::try_from(proto).expect("identity round-trips");
        let mut expected = original;
        expected.legacy_exe_sha256 = [0; 32];
        assert_eq!(
            restored, expected,
            "the canonical BLAKE3 identity must survive the manifest round-trip; the legacy probe field is not persisted"
        );
    }

    #[test]
    fn legacy_sha256_wire_identity_is_rejected_actionably() {
        // Pre-#946 peers populated reserved field 3 and know nothing about the
        // new algorithm/hash fields. Prost drops the unknown legacy field, so
        // the explicit empty algorithm marker is what turns version skew into
        // a contract error instead of a false executable mismatch.
        let legacy = protocol::DaemonProcess {
            pid: 1234,
            exe_path: "legacy-daemon".to_owned(),
            exe_hash_algorithm: String::new(),
            exe_hash: Vec::new(),
            ipc_endpoint: Some(endpoint("legacy.sock")),
            started_at_unix_ms: 1,
            boot_id: "boot-1".to_owned(),
            idle_timeout_secs: None,
        };

        let error = DaemonProcess::try_from(legacy).expect_err("legacy SHA-256 must be fenced");
        assert!(matches!(
            error,
            IdentityError::UnsupportedExecutableHashAlgorithm(ref algorithm)
                if algorithm.is_empty()
        ));
        assert!(error.to_string().contains("expected blake3"));
    }
}
