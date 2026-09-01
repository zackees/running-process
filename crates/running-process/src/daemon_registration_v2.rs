//! Frozen v2 service-definition writer substrate.
//!
//! This direct surface owns only v2 `.servicedef.v2` construction, validation,
//! location, and persistence. It deliberately excludes v2 manifests, loading,
//! broker negotiation, endpoint transport, and runtime policy. The legacy
//! client-only `broker::protocol_v2` path re-exports these exact items.

use std::path::{Path, PathBuf};

use prost::Message as _;

use crate::daemon_registration_common::service_definition::{
    ensure_service_definition_dir, service_definition_dir,
};

/// Shared validation error and service-name validation used by v1 and v2.
pub use crate::daemon_registration_common::{
    service_definition::ServiceDefinitionError,
    validation::{validate_service_name, PipePathError},
};
/// Generated v2 service-definition types needed by the writer.
pub use running_process_protocol::broker::v2::{BrokerIsolation, ServiceDefinition};

/// v2 service-definition file extension. Distinct from v1's `servicedef`
/// so the two records can coexist in one owner-private directory.
pub const SERVICE_DEF_V2_EXTENSION: &str = "servicedef.v2";

/// Return the v2 service-definition directory.
///
/// This is the exact v1 platform/env-selected root; only the file extension
/// differs during the rollout.
#[must_use]
pub fn service_definition_dir_v2() -> PathBuf {
    service_definition_dir()
}

/// Compute the v2 path for one service definition.
///
/// # Errors
///
/// Returns [`ServiceDefinitionError::InvalidName`] when the name fails the
/// frozen `[a-z0-9-]{1,64}` validation shared with v1.
pub fn service_definition_path_v2(
    root: &Path,
    service_name: &str,
) -> Result<PathBuf, ServiceDefinitionError> {
    validate_service_name(service_name)?;
    Ok(root.join(format!("{service_name}.{SERVICE_DEF_V2_EXTENSION}")))
}

/// Validate the service name and write one `.servicedef.v2` file into `root`.
///
/// The established writer intentionally uses one direct `std::fs::write`.
/// It is non-atomic; callers that require a different persistence policy own
/// that policy above this frozen compatibility layer.
///
/// # Errors
///
/// Returns I/O, invalid-name, or insecure-directory errors from the shared
/// service-definition path and owner-private directory policy.
pub fn write_service_definition_v2(
    root: &Path,
    definition: &ServiceDefinition,
) -> Result<PathBuf, ServiceDefinitionError> {
    ensure_service_definition_dir(root)?;
    let path = service_definition_path_v2(root, &definition.service_name)?;
    std::fs::write(&path, definition.encode_to_vec())?;
    Ok(path)
}

/// Builder for a generated v2 [`ServiceDefinition`].
///
/// The builder preserves the existing version-list order and inserts labels in
/// the generated `HashMap`; it intentionally neither sorts nor canonicalizes
/// labels. It does not set the optional v2 HTTP capability.
#[derive(Debug, Clone)]
pub struct ServiceDefinitionBuilder {
    definition: ServiceDefinition,
}

impl ServiceDefinitionBuilder {
    /// Start a definition for the per-user shared broker.
    #[must_use]
    pub fn shared_broker(service_name: impl Into<String>, binary_path: impl Into<String>) -> Self {
        Self {
            definition: ServiceDefinition {
                service_name: service_name.into(),
                binary_path: binary_path.into(),
                isolation: BrokerIsolation::SharedBroker as i32,
                ..Default::default()
            },
        }
    }

    /// Start a definition for a private per-service broker.
    #[must_use]
    pub fn private_broker(service_name: impl Into<String>, binary_path: impl Into<String>) -> Self {
        Self {
            definition: ServiceDefinition {
                service_name: service_name.into(),
                binary_path: binary_path.into(),
                isolation: BrokerIsolation::PrivateBroker as i32,
                ..Default::default()
            },
        }
    }

    /// Start a definition pinned to a named broker instance.
    #[must_use]
    pub fn explicit_instance(
        service_name: impl Into<String>,
        binary_path: impl Into<String>,
        instance: impl Into<String>,
    ) -> Self {
        Self {
            definition: ServiceDefinition {
                service_name: service_name.into(),
                binary_path: binary_path.into(),
                isolation: BrokerIsolation::ExplicitInstance as i32,
                explicit_instance: instance.into(),
                ..Default::default()
            },
        }
    }

    /// Pin the canonicalized binary-directory allow-list root.
    #[must_use]
    pub fn per_version_binary_dir(mut self, dir: impl Into<String>) -> Self {
        self.definition.per_version_binary_dir = dir.into();
        self
    }

    /// Set the semver floor.
    #[must_use]
    pub fn min_version(mut self, version: impl Into<String>) -> Self {
        self.definition.min_version = version.into();
        self
    }

    /// Replace the version allow-list, retaining the caller's iteration order.
    #[must_use]
    pub fn version_allow_list<I, S>(mut self, versions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.definition.version_allow_list = versions.into_iter().map(Into::into).collect();
        self
    }

    /// Insert one label into the generated protobuf map.
    #[must_use]
    pub fn label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.definition.labels.insert(key.into(), value.into());
        self
    }

    /// Finalize the generated definition without adding validation policy.
    #[must_use]
    pub fn build(self) -> ServiceDefinition {
        self.definition
    }

    /// Install into an explicit service-definition root.
    pub fn install_in(self, root: &Path) -> Result<PathBuf, ServiceDefinitionError> {
        write_service_definition_v2(root, &self.build())
    }

    /// Install into the established platform/env-selected root.
    pub fn install(self) -> Result<PathBuf, ServiceDefinitionError> {
        let root = service_definition_dir_v2();
        crate::daemon_registration_common::secure_dir::ensure_private_dir(&root)?;
        self.install_in(&root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn extension_is_servicedef_v2() {
        assert_eq!(SERVICE_DEF_V2_EXTENSION, "servicedef.v2");
    }

    #[test]
    fn service_definition_path_v2_uses_v2_extension() {
        let root = Path::new("/svc");
        let path = service_definition_path_v2(root, "zccache").expect("valid path");
        assert_eq!(
            path.to_string_lossy().replace('\\', "/"),
            "/svc/zccache.servicedef.v2"
        );
    }

    #[test]
    fn service_definition_path_v2_rejects_invalid_name() {
        let root = Path::new("/svc");
        assert!(service_definition_path_v2(root, "ZCCACHE").is_err());
        assert!(service_definition_path_v2(root, "").is_err());
        assert!(service_definition_path_v2(root, "a/b").is_err());
    }

    #[test]
    fn shared_broker_builder_sets_expected_fields() {
        let definition =
            ServiceDefinitionBuilder::shared_broker("zccache", "/usr/bin/zccache").build();
        assert_eq!(definition.service_name, "zccache");
        assert_eq!(definition.binary_path, "/usr/bin/zccache");
        assert_eq!(definition.isolation, BrokerIsolation::SharedBroker as i32);
        assert!(definition.explicit_instance.is_empty());
        assert!(definition.http_server.is_none());
    }

    #[test]
    fn private_broker_builder_sets_expected_fields() {
        let definition = ServiceDefinitionBuilder::private_broker("svc", "/bin/x").build();
        assert_eq!(definition.isolation, BrokerIsolation::PrivateBroker as i32);
    }

    #[test]
    fn explicit_instance_builder_sets_expected_fields() {
        let definition =
            ServiceDefinitionBuilder::explicit_instance("svc", "/bin/x", "ci-trusted").build();
        assert_eq!(
            definition.isolation,
            BrokerIsolation::ExplicitInstance as i32
        );
        assert_eq!(definition.explicit_instance, "ci-trusted");
    }

    #[test]
    fn builder_chain_propagates_optional_fields() {
        let definition = ServiceDefinitionBuilder::shared_broker("svc", "/bin/x")
            .per_version_binary_dir("/usr/local/bin")
            .min_version("1.2.3")
            .version_allow_list(["1.2.3", "1.3.0"])
            .label("env", "prod")
            .label("region", "us-west")
            .build();
        assert_eq!(definition.per_version_binary_dir, "/usr/local/bin");
        assert_eq!(definition.min_version, "1.2.3");
        assert_eq!(definition.version_allow_list, vec!["1.2.3", "1.3.0"]);
        assert_eq!(definition.labels.get("env"), Some(&"prod".to_owned()));
        assert_eq!(definition.labels.get("region"), Some(&"us-west".to_owned()));
        assert!(definition.http_server.is_none());
    }

    #[test]
    fn install_in_writes_and_decodes_round_trip() {
        let directory = tempdir().expect("tempdir");
        let path = ServiceDefinitionBuilder::shared_broker("zccache", "/usr/bin/zccache")
            .min_version("1.0.0")
            .label("env", "prod")
            .install_in(directory.path())
            .expect("install");

        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("zccache.servicedef.v2")
        );
        let decoded =
            ServiceDefinition::decode(std::fs::read(&path).expect("read definition").as_slice())
                .expect("decode definition");
        assert_eq!(decoded.service_name, "zccache");
        assert_eq!(decoded.binary_path, "/usr/bin/zccache");
        assert_eq!(decoded.isolation, BrokerIsolation::SharedBroker as i32);
        assert_eq!(decoded.min_version, "1.0.0");
        assert_eq!(decoded.labels.get("env"), Some(&"prod".to_owned()));
        assert!(decoded.http_server.is_none());
    }

    #[test]
    fn write_service_definition_v2_rejects_invalid_name() {
        let directory = tempdir().expect("tempdir");
        let bad = ServiceDefinition {
            service_name: "BAD-Caps".to_owned(),
            ..Default::default()
        };
        assert!(write_service_definition_v2(directory.path(), &bad).is_err());
    }

    #[test]
    fn write_service_definition_v2_creates_parent_dir() {
        let directory = tempdir().expect("tempdir");
        let nested = directory.path().join("nested");
        let path = ServiceDefinitionBuilder::shared_broker("svc", "/bin/x")
            .install_in(&nested)
            .expect("install into nested");
        assert!(path.exists());
        assert!(nested.exists());
    }

    #[test]
    fn builder_install_round_trip_preserves_every_field() {
        let directory = tempdir().expect("tempdir");
        let path = ServiceDefinitionBuilder::explicit_instance("svc", "/bin/x", "ci-trusted")
            .per_version_binary_dir("/usr/local/bin")
            .min_version("1.0.0")
            .version_allow_list(["1.0.0", "1.1.0"])
            .label("env", "prod")
            .label("rollout", "blue")
            .install_in(directory.path())
            .expect("install");

        let decoded =
            ServiceDefinition::decode(std::fs::read(&path).expect("read definition").as_slice())
                .expect("decode definition");
        assert_eq!(decoded.service_name, "svc");
        assert_eq!(decoded.binary_path, "/bin/x");
        assert_eq!(decoded.isolation, BrokerIsolation::ExplicitInstance as i32);
        assert_eq!(decoded.explicit_instance, "ci-trusted");
        assert_eq!(decoded.per_version_binary_dir, "/usr/local/bin");
        assert_eq!(decoded.min_version, "1.0.0");
        assert_eq!(decoded.version_allow_list, vec!["1.0.0", "1.1.0"]);
        assert_eq!(decoded.labels.get("env"), Some(&"prod".to_owned()));
        assert_eq!(decoded.labels.get("rollout"), Some(&"blue".to_owned()));
        assert!(decoded.http_server.is_none());
    }
}

#[cfg(all(test, feature = "client"))]
mod compatibility_tests {
    use std::any::TypeId;

    use prost::Message as _;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn legacy_client_v2_writer_paths_reexport_canonical_types_and_bytes() {
        assert_eq!(
            TypeId::of::<ServiceDefinition>(),
            TypeId::of::<crate::broker::protocol_v2::ServiceDefinition>(),
        );
        assert_eq!(
            TypeId::of::<BrokerIsolation>(),
            TypeId::of::<crate::broker::protocol_v2::BrokerIsolation>(),
        );
        assert_eq!(
            TypeId::of::<ServiceDefinitionBuilder>(),
            TypeId::of::<crate::broker::protocol_v2::ServiceDefinitionBuilder>(),
        );
        assert_eq!(
            TypeId::of::<ServiceDefinitionError>(),
            TypeId::of::<crate::broker::server::service_def_loader::ServiceDefinitionError>(),
        );

        let definition = ServiceDefinitionBuilder::shared_broker("zccache", "/bin/zccache")
            .per_version_binary_dir("/bin")
            .min_version("1.2.3")
            .version_allow_list(["1.2.3", "1.2.4"])
            .label("package", "zccache")
            .label("vendor", "zackees")
            .build();
        let canonical_root = tempdir().expect("canonical root");
        let legacy_root = tempdir().expect("legacy root");
        let canonical_path = write_service_definition_v2(canonical_root.path(), &definition)
            .expect("canonical write");
        let legacy_path = crate::broker::protocol_v2::write_service_definition_v2(
            legacy_root.path(),
            &definition,
        )
        .expect("legacy write");

        let canonical_bytes = std::fs::read(canonical_path).expect("canonical bytes");
        let legacy_bytes = std::fs::read(legacy_path).expect("legacy bytes");
        assert_eq!(canonical_bytes, legacy_bytes);

        let decoded = ServiceDefinition::decode(canonical_bytes.as_slice()).expect("decode bytes");
        assert_eq!(decoded, definition);
        assert!(decoded.http_server.is_none());
    }
}
