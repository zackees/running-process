//! Frozen v1 daemon-registration semantics for application-owned policy.
//!
//! The private substrate retains the established v1 record bytes, manifest
//! seal, and owner-private persistence behavior. This facade deliberately
//! does not select an endpoint, construct a broker client, or define product
//! cache/service policy.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::daemon_registration as backend;

/// The v1 cache-root categories used by real registration consumers.
///
/// [`Self::Unknown`] preserves a decoded additive wire value without exposing
/// a generated protocol enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheRootKind {
    /// Durable cache artifact data.
    CacheData,
    /// Dependency or artifact index data.
    CacheIndex,
    /// Cache log files.
    CacheLogs,
    /// Cache coordination/lock files.
    CacheLocks,
    /// Temporary cache files.
    CacheTmp,
    /// A frozen v1 root-kind value not yet assigned facade semantics.
    Unknown(i32),
}

impl CacheRootKind {
    fn from_backend(value: i32) -> Self {
        match backend::protocol::CacheRootKind::try_from(value) {
            Ok(backend::protocol::CacheRootKind::CacheData) => Self::CacheData,
            Ok(backend::protocol::CacheRootKind::CacheIndex) => Self::CacheIndex,
            Ok(backend::protocol::CacheRootKind::CacheLogs) => Self::CacheLogs,
            Ok(backend::protocol::CacheRootKind::CacheLocks) => Self::CacheLocks,
            Ok(backend::protocol::CacheRootKind::CacheTmp) => Self::CacheTmp,
            Ok(_) | Err(_) => Self::Unknown(value),
        }
    }

    fn into_backend(self) -> i32 {
        match self {
            Self::CacheData => backend::protocol::CacheRootKind::CacheData as i32,
            Self::CacheIndex => backend::protocol::CacheRootKind::CacheIndex as i32,
            Self::CacheLogs => backend::protocol::CacheRootKind::CacheLogs as i32,
            Self::CacheLocks => backend::protocol::CacheRootKind::CacheLocks as i32,
            Self::CacheTmp => backend::protocol::CacheRootKind::CacheTmp as i32,
            Self::Unknown(value) => value,
        }
    }
}

/// A borrowed, ordered cache-root entry in a decoded manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheRoot<'a> {
    kind: CacheRootKind,
    path: &'a str,
}

impl<'a> CacheRoot<'a> {
    /// Semantic root category.
    #[must_use]
    pub fn kind(self) -> CacheRootKind {
        self.kind
    }

    /// Root path as it was recorded in the frozen v1 record.
    #[must_use]
    pub fn path(self) -> &'a str {
        self.path
    }
}

/// Failure while validating, persisting, or reading a frozen v1 registration.
#[derive(Debug)]
pub enum DaemonRegistrationError {
    /// A filesystem operation failed.
    Io(std::io::Error),
    /// A persisted v1 record could not be decoded.
    MalformedRecord,
    /// A persisted manifest's required SHA-256 seal did not verify.
    ManifestIntegrityFailure,
    /// A manifest uses a schema newer than this facade supports.
    UnsupportedManifestSchema {
        /// Schema found in the persisted manifest.
        got: u32,
        /// Maximum schema this facade supports.
        supported: u32,
    },
    /// A service name or version violates frozen v1 validation.
    InvalidNameOrVersion {
        /// Stable diagnostic supplied by the underlying validator.
        detail: String,
    },
    /// A required registration path had no parent directory.
    MissingParent {
        /// Path rejected by the persistence layer.
        path: PathBuf,
    },
    /// A registration directory was not private to its current user.
    InsecureDirectory {
        /// Directory rejected by the persistence layer.
        path: PathBuf,
    },
    /// A loaded service-definition did not name the requested service.
    ServiceNameMismatch {
        /// Service name requested by the caller.
        requested: String,
        /// Service name stored in the record.
        actual: String,
    },
    /// A service-definition binary or per-version directory was invalid.
    InvalidServicePath {
        /// Field rejected by frozen v1 validation.
        field: &'static str,
        /// Recorded path string.
        path: String,
        /// Stable validation reason.
        reason: &'static str,
    },
    /// A service-definition isolation combination was invalid.
    InvalidServiceIsolation {
        /// Stable validation reason.
        reason: &'static str,
    },
    /// Serializing a v1 record failed.
    Serialization,
}

impl fmt::Display for DaemonRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "daemon registration I/O failed: {error}"),
            Self::MalformedRecord => formatter.write_str("malformed frozen v1 registration record"),
            Self::ManifestIntegrityFailure => {
                formatter.write_str("frozen v1 manifest SHA-256 seal did not verify")
            }
            Self::UnsupportedManifestSchema { got, supported } => {
                write!(
                    formatter,
                    "unsupported manifest schema {got}; supported through {supported}"
                )
            }
            Self::InvalidNameOrVersion { detail } => {
                write!(
                    formatter,
                    "invalid frozen v1 service name or version: {detail}"
                )
            }
            Self::MissingParent { path } => {
                write!(
                    formatter,
                    "registration path has no parent: {}",
                    path.display()
                )
            }
            Self::InsecureDirectory { path } => {
                write!(
                    formatter,
                    "registration directory is not owner-private: {}",
                    path.display()
                )
            }
            Self::ServiceNameMismatch { requested, actual } => {
                write!(
                    formatter,
                    "requested service {requested:?}, found {actual:?}"
                )
            }
            Self::InvalidServicePath {
                field,
                path,
                reason,
            } => write!(
                formatter,
                "invalid service-definition {field} {path:?}: {reason}"
            ),
            Self::InvalidServiceIsolation { reason } => {
                write!(formatter, "invalid service-definition isolation: {reason}")
            }
            Self::Serialization => {
                formatter.write_str("frozen v1 registration serialization failed")
            }
        }
    }
}

impl Error for DaemonRegistrationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

fn manifest_error(error: backend::manifest::ManifestError) -> DaemonRegistrationError {
    match error {
        backend::manifest::ManifestError::Io(error) => DaemonRegistrationError::Io(error),
        backend::manifest::ManifestError::Decode(_) => DaemonRegistrationError::MalformedRecord,
        backend::manifest::ManifestError::Encode(_) => DaemonRegistrationError::Serialization,
        backend::manifest::ManifestError::Corruption => {
            DaemonRegistrationError::ManifestIntegrityFailure
        }
        backend::manifest::ManifestError::SchemaTooNew { got, supported } => {
            DaemonRegistrationError::UnsupportedManifestSchema { got, supported }
        }
        backend::manifest::ManifestError::InvalidName(error) => {
            DaemonRegistrationError::InvalidNameOrVersion {
                detail: error.to_string(),
            }
        }
        backend::manifest::ManifestError::MissingParent(path) => {
            DaemonRegistrationError::MissingParent { path }
        }
        backend::manifest::ManifestError::InsecureRegistry(path) => {
            DaemonRegistrationError::InsecureDirectory { path }
        }
    }
}

fn service_error(
    error: backend::service_def_loader::ServiceDefinitionError,
) -> DaemonRegistrationError {
    match error {
        backend::service_def_loader::ServiceDefinitionError::Io(error) => {
            DaemonRegistrationError::Io(error)
        }
        backend::service_def_loader::ServiceDefinitionError::Decode(_) => {
            DaemonRegistrationError::MalformedRecord
        }
        backend::service_def_loader::ServiceDefinitionError::InvalidName(error) => {
            DaemonRegistrationError::InvalidNameOrVersion {
                detail: error.to_string(),
            }
        }
        backend::service_def_loader::ServiceDefinitionError::InsecureDirectory(path) => {
            DaemonRegistrationError::InsecureDirectory { path }
        }
        backend::service_def_loader::ServiceDefinitionError::ServiceNameMismatch {
            requested,
            actual,
        } => DaemonRegistrationError::ServiceNameMismatch { requested, actual },
        backend::service_def_loader::ServiceDefinitionError::InvalidPath {
            field,
            path,
            reason,
        } => DaemonRegistrationError::InvalidServicePath {
            field,
            path,
            reason,
        },
        backend::service_def_loader::ServiceDefinitionError::InvalidIsolation { reason } => {
            DaemonRegistrationError::InvalidServiceIsolation { reason }
        }
    }
}

/// Builder for a frozen v1 cache manifest.
#[derive(Clone, Debug)]
pub struct CacheManifestBuilder {
    inner: backend::builders::CacheManifestBuilder,
    roots: Vec<(i32, String)>,
}

impl CacheManifestBuilder {
    /// Begin a manifest for this application-selected service and version.
    #[must_use]
    pub fn new(service_name: impl Into<String>, service_version: impl Into<String>) -> Self {
        Self {
            inner: backend::builders::CacheManifestBuilder::new(service_name, service_version),
            roots: Vec::new(),
        }
    }

    /// Record the application-selected broker-instance label.
    #[must_use]
    pub fn broker_instance(mut self, instance: impl Into<String>) -> Self {
        self.inner = self.inner.broker_instance(instance);
        self
    }

    /// Append one root, retaining caller order in the v1 record.
    #[must_use]
    pub fn root(mut self, kind: CacheRootKind, path: impl Into<String>) -> Self {
        self.roots.push((kind.into_backend(), path.into()));
        self
    }

    /// Build and SHA-256 seal this manifest without persisting it.
    pub fn build(self) -> Result<CacheManifest, DaemonRegistrationError> {
        let mut manifest = self.inner.build().map_err(manifest_error)?;
        manifest.roots = self
            .roots
            .into_iter()
            .map(|(kind, path)| backend::protocol::CacheRoot {
                kind,
                path,
                ..Default::default()
            })
            .collect();
        backend::manifest::manifest_with_self_sha256(&manifest)
            .map(CacheManifest::from_backend)
            .map_err(manifest_error)
    }

    /// Build, seal, and atomically publish this manifest in the default registry.
    pub fn publish(self) -> Result<PathBuf, DaemonRegistrationError> {
        let manifest = self.build()?;
        backend::manifest::write_to_central(
            manifest.service_name(),
            manifest.service_version(),
            &manifest.inner,
        )
        .map_err(manifest_error)
    }

    /// Build, seal, and atomically publish this manifest in `registry_dir`.
    pub fn publish_in(
        self,
        registry_dir: impl AsRef<Path>,
    ) -> Result<PathBuf, DaemonRegistrationError> {
        let manifest = self.build()?;
        backend::manifest::write_to_central_in_dir(
            registry_dir.as_ref(),
            manifest.service_name(),
            manifest.service_version(),
            &manifest.inner,
        )
        .map_err(manifest_error)
    }
}

/// A frozen v1 cache manifest with its generated record kept private.
#[derive(Clone, Debug, PartialEq)]
pub struct CacheManifest {
    inner: backend::protocol::CacheManifest,
}

impl CacheManifest {
    fn from_backend(inner: backend::protocol::CacheManifest) -> Self {
        Self { inner }
    }

    /// Read, schema-check, and verify a sealed frozen v1 manifest.
    pub fn read(path: impl AsRef<Path>) -> Result<Self, DaemonRegistrationError> {
        backend::manifest::read_manifest(path.as_ref())
            .map(Self::from_backend)
            .map_err(manifest_error)
    }

    /// Service name retained in the v1 record.
    #[must_use]
    pub fn service_name(&self) -> &str {
        &self.inner.service_name
    }

    /// Service version retained in the v1 record.
    #[must_use]
    pub fn service_version(&self) -> &str {
        &self.inner.service_version
    }

    /// Broker envelope version retained in the v1 record.
    #[must_use]
    pub fn broker_envelope_version(&self) -> &str {
        &self.inner.broker_envelope_version
    }

    /// Application-selected broker-instance label.
    #[must_use]
    pub fn broker_instance(&self) -> &str {
        &self.inner.broker_instance
    }

    /// Frozen v1 schema number.
    #[must_use]
    pub fn schema_version(&self) -> u32 {
        self.inner.manifest_schema_version
    }

    /// Frozen v1 media type.
    #[must_use]
    pub fn media_type(&self) -> &str {
        &self.inner.media_type
    }

    /// Unix-millisecond creation time stamped by the v1 builder.
    #[must_use]
    pub fn created_at_unix_ms(&self) -> u64 {
        self.inner.created_at_unix_ms
    }

    /// Unix-millisecond last-active time stamped by the v1 builder.
    #[must_use]
    pub fn last_active_unix_ms(&self) -> u64 {
        self.inner.last_active_unix_ms
    }

    /// Whether the v1 builder stamped host identity metadata.
    #[must_use]
    pub fn has_host_identity(&self) -> bool {
        self.inner.host.is_some()
    }

    /// Whether the manifest carries a correctly sized v1 seal.
    #[must_use]
    pub fn has_sha256_seal(&self) -> bool {
        self.inner.self_sha256.len() == 32
    }

    /// Ordered cache roots retained by the manifest.
    pub fn roots(&self) -> impl ExactSizeIterator<Item = CacheRoot<'_>> {
        self.inner.roots.iter().map(|root| CacheRoot {
            kind: CacheRootKind::from_backend(root.kind),
            path: &root.path,
        })
    }
}

/// Builder for the shared-broker frozen v1 service definition used by current consumers.
#[derive(Clone, Debug)]
pub struct ServiceDefinitionBuilder {
    inner: backend::builders::ServiceDefinitionBuilder,
}

impl ServiceDefinitionBuilder {
    /// Begin a shared-broker definition for an application-selected binary.
    #[must_use]
    pub fn shared_broker(service_name: impl Into<String>, binary_path: impl Into<String>) -> Self {
        Self {
            inner: backend::builders::ServiceDefinitionBuilder::shared_broker(
                service_name,
                binary_path,
            ),
        }
    }

    /// Set the absolute directory containing per-version binaries.
    #[must_use]
    pub fn per_version_binary_dir(mut self, directory: impl Into<String>) -> Self {
        self.inner = self.inner.per_version_binary_dir(directory);
        self
    }

    /// Set the minimum compatible service version.
    #[must_use]
    pub fn min_version(mut self, version: impl Into<String>) -> Self {
        self.inner = self.inner.min_version(version);
        self
    }

    /// Append one allowed service version, retaining caller order.
    #[must_use]
    pub fn allow_version(mut self, version: impl Into<String>) -> Self {
        self.inner = self.inner.allow_version(version);
        self
    }

    /// Attach one application-selected diagnostic label.
    #[must_use]
    pub fn label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.inner = self.inner.label(key, value);
        self
    }

    /// Validate and return a facade-owned v1 definition without persisting it.
    pub fn build(self) -> Result<ServiceDefinition, DaemonRegistrationError> {
        self.inner
            .build()
            .map(ServiceDefinition::from_backend)
            .map_err(service_error)
    }

    /// Validate and write the v1 definition into the default owner-private directory.
    pub fn install(self) -> Result<PathBuf, DaemonRegistrationError> {
        self.inner.install().map_err(service_error)
    }

    /// Validate and write the v1 definition into an explicit owner-private directory.
    pub fn install_in(self, root: impl AsRef<Path>) -> Result<PathBuf, DaemonRegistrationError> {
        self.inner.install_in(root.as_ref()).map_err(service_error)
    }
}

/// A frozen v1 service definition with its generated record kept private.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceDefinition {
    inner: backend::protocol::ServiceDefinition,
}

impl ServiceDefinition {
    fn from_backend(inner: backend::protocol::ServiceDefinition) -> Self {
        Self { inner }
    }

    /// Read and validate a v1 definition for `service_name` from `root`.
    pub fn read(
        root: impl AsRef<Path>,
        service_name: impl AsRef<str>,
    ) -> Result<Self, DaemonRegistrationError> {
        backend::service_def_loader::ServiceDefinitionLoader::new(root.as_ref())
            .load(service_name.as_ref())
            .map(Self::from_backend)
            .map_err(service_error)
    }

    /// Service name retained in the v1 definition.
    #[must_use]
    pub fn service_name(&self) -> &str {
        &self.inner.service_name
    }

    /// Absolute binary path string retained in the v1 definition.
    #[must_use]
    pub fn binary_path(&self) -> &str {
        &self.inner.binary_path
    }

    /// Whether this is the shared-broker form used by current consumers.
    #[must_use]
    pub fn is_shared_broker(&self) -> bool {
        self.inner.isolation == backend::protocol::BrokerIsolation::SharedBroker as i32
    }

    /// Absolute directory holding per-version binaries, when set.
    #[must_use]
    pub fn per_version_binary_dir(&self) -> &str {
        &self.inner.per_version_binary_dir
    }

    /// Minimum compatible service version, when set.
    #[must_use]
    pub fn min_version(&self) -> &str {
        &self.inner.min_version
    }

    /// Ordered allowed service versions.
    pub fn allowed_versions(&self) -> impl ExactSizeIterator<Item = &str> {
        self.inner.version_allow_list.iter().map(String::as_str)
    }

    /// Value for one diagnostic label, if present.
    #[must_use]
    pub fn label(&self, key: &str) -> Option<&str> {
        self.inner.labels.get(key).map(String::as_str)
    }
}

/// The default owner-private directory for frozen v1 cache manifests.
#[must_use]
pub fn manifest_directory() -> PathBuf {
    backend::manifest::central_registry_dir()
}

/// The default owner-private directory for frozen v1 service definitions.
#[must_use]
pub fn service_definition_directory() -> PathBuf {
    backend::service_def_loader::service_definition_dir()
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;

    const FROZEN_MANIFEST_V1: &[u8] = &[
        0x0a, 0x07, b'z', b'c', b'c', b'a', b'c', b'h', b'e', 0x12, 0x05, b'1', b'.', b'2', b'.',
        b'3', 0x1a, 0x02, b'v', b'1', 0x20, 0x01, 0x28, 0x02, 0xc2, 0x02, 0x06, b's', b'h', b'a',
        b'r', b'e', b'd', 0xb2, 0x04, 0x06, b'b', b'u', b'n', b'd', b'l', b'e', 0xa0, 0x06, 0x01,
        0xaa, 0x06, 0x31, b'a', b'p', b'p', b'l', b'i', b'c', b'a', b't', b'i', b'o', b'n', b'/',
        b'v', b'n', b'd', b'.', b'r', b'u', b'n', b'n', b'i', b'n', b'g', b'-', b'p', b'r', b'o',
        b'c', b'e', b's', b's', b'.', b'c', b'a', b'c', b'h', b'e', b'-', b'm', b'a', b'n', b'i',
        b'f', b'e', b's', b't', b'.', b'v', b'1', 0xb2, 0x06, 0x20, 0x01, 0x12, 0x0d, 0x59, 0xff,
        0xa9, 0x45, 0xe3, 0xff, 0xa4, 0x6a, 0xaa, 0xaf, 0xee, 0xc8, 0x6f, 0xef, 0xfe, 0x55, 0xc2,
        0x5f, 0x0a, 0x04, 0x0c, 0x9d, 0xe3, 0xdb, 0x67, 0x4b, 0xe3, 0xa0, 0x51,
    ];

    #[test]
    fn reads_the_frozen_v1_manifest_golden_without_protocol_types() {
        let directory = tempfile::tempdir().expect("manifest tempdir");
        let path = directory.path().join("zccache-1.2.3.pb");
        std::fs::write(&path, FROZEN_MANIFEST_V1).expect("write frozen manifest");

        let manifest = CacheManifest::read(&path).expect("read frozen manifest");
        assert_eq!(manifest.service_name(), "zccache");
        assert_eq!(manifest.service_version(), "1.2.3");
        assert_eq!(manifest.broker_envelope_version(), "v1");
        assert_eq!(manifest.broker_instance(), "shared");
        assert_eq!(manifest.schema_version(), 1);
        assert_eq!(
            manifest.media_type(),
            "application/vnd.running-process.cache-manifest.v1"
        );
        assert_eq!(manifest.created_at_unix_ms(), 1);
        assert_eq!(manifest.last_active_unix_ms(), 2);
        assert!(manifest.has_sha256_seal());
    }

    #[test]
    fn frozen_manifest_rejects_payload_changes_under_the_original_seal() {
        let directory = tempfile::tempdir().expect("manifest tempdir");
        let path = directory.path().join("tampered.pb");
        let mut bytes = FROZEN_MANIFEST_V1.to_vec();
        // Change a valid service-name byte without changing the protobuf shape
        // or stored seal. This must be integrity failure, not decode failure.
        assert_eq!(bytes[2], b'z');
        bytes[2] = b'x';
        std::fs::write(&path, bytes).expect("write tampered manifest");
        assert!(matches!(
            CacheManifest::read(&path),
            Err(DaemonRegistrationError::ManifestIntegrityFailure)
        ));
    }

    #[test]
    fn unknown_cache_root_discriminants_and_borrowed_paths_survive() {
        for raw in [-7, 713, i32::MAX] {
            assert_eq!(
                CacheRootKind::from_backend(raw),
                CacheRootKind::Unknown(raw)
            );
            assert_eq!(CacheRootKind::from_backend(raw).into_backend(), raw);
        }
        let path = String::from("/cache/retained");
        let root = CacheRoot {
            kind: CacheRootKind::Unknown(713),
            path: &path,
        };
        assert_eq!(root.kind(), CacheRootKind::Unknown(713));
        assert_eq!(root.path().as_ptr(), path.as_ptr());
    }

    #[test]
    fn integrity_schema_and_io_failures_keep_distinct_classifications() {
        assert!(matches!(
            manifest_error(backend::manifest::ManifestError::Corruption),
            DaemonRegistrationError::ManifestIntegrityFailure
        ));
        assert!(matches!(
            manifest_error(backend::manifest::ManifestError::SchemaTooNew {
                got: 99,
                supported: 1,
            }),
            DaemonRegistrationError::UnsupportedManifestSchema {
                got: 99,
                supported: 1
            }
        ));
        let error = manifest_error(backend::manifest::ManifestError::Io(
            std::io::Error::from_raw_os_error(13),
        ));
        assert!(error.source().is_some());
        let DaemonRegistrationError::Io(source) = error else {
            panic!("I/O error expected")
        };
        assert_eq!(source.raw_os_error(), Some(13));
    }
}
