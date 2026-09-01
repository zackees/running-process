#![cfg(feature = "daemon-registration")]

//! No-default public contract for frozen v1 daemon registration.

use std::fs;

use prost::Message;
use running_process::daemon_registration::{
    builders::{CacheManifestBuilder, ServiceDefinitionBuilder},
    manifest::{
        central_registry_dir, manifest_with_self_sha256, read_manifest, scan_central,
        ManifestError, CACHE_MANIFEST_MEDIA_TYPE,
    },
    protocol::{BrokerIsolation, CacheManifest, CacheRootKind, ServiceDefinition},
    service_def_loader::{
        service_definition_dir, service_definition_path, write_service_definition,
        ServiceDefinitionLoader, SERVICE_DEF_EXTENSION,
    },
    validation::{validate_service_name, validate_version},
};

#[cfg(windows)]
const ABS_BINARY: &str = "C:\\tools\\zccache.exe";
#[cfg(not(windows))]
const ABS_BINARY: &str = "/usr/local/bin/zccache";

#[test]
fn manifest_sha256_uses_the_frozen_v1_protobuf_bytes() {
    let sealed = manifest_with_self_sha256(&CacheManifest {
        manifest_schema_version: 1,
        media_type: CACHE_MANIFEST_MEDIA_TYPE.into(),
        service_name: "zccache".into(),
        service_version: "1.2.3".into(),
        broker_envelope_version: "v1".into(),
        created_at_unix_ms: 1,
        last_active_unix_ms: 2,
        broker_instance: "shared".into(),
        bundle_id: "bundle".into(),
        ..Default::default()
    })
    .expect("seal manifest");

    assert_eq!(
        sealed.encode_to_vec(),
        vec![
            0x0a, 0x07, b'z', b'c', b'c', b'a', b'c', b'h', b'e', 0x12, 0x05, b'1', b'.', b'2',
            b'.', b'3', 0x1a, 0x02, b'v', b'1', 0x20, 0x01, 0x28, 0x02, 0xc2, 0x02, 0x06, b's',
            b'h', b'a', b'r', b'e', b'd', 0xb2, 0x04, 0x06, b'b', b'u', b'n', b'd', b'l', b'e',
            0xa0, 0x06, 0x01, 0xaa, 0x06, 0x31, b'a', b'p', b'p', b'l', b'i', b'c', b'a', b't',
            b'i', b'o', b'n', b'/', b'v', b'n', b'd', b'.', b'r', b'u', b'n', b'n', b'i', b'n',
            b'g', b'-', b'p', b'r', b'o', b'c', b'e', b's', b's', b'.', b'c', b'a', b'c', b'h',
            b'e', b'-', b'm', b'a', b'n', b'i', b'f', b'e', b's', b't', b'.', b'v', b'1', 0xb2,
            0x06, 0x20, 0x01, 0x12, 0x0d, 0x59, 0xff, 0xa9, 0x45, 0xe3, 0xff, 0xa4, 0x6a, 0xaa,
            0xaf, 0xee, 0xc8, 0x6f, 0xef, 0xfe, 0x55, 0xc2, 0x5f, 0x0a, 0x04, 0x0c, 0x9d, 0xe3,
            0xdb, 0x67, 0x4b, 0xe3, 0xa0, 0x51,
        ],
    );
}

#[test]
fn builders_stamp_host_and_round_trip_through_private_registry() {
    let registry = tempfile::tempdir().expect("registry tempdir");
    let path = CacheManifestBuilder::new("zccache", "1.11.20")
        .broker_instance("shared")
        .root(CacheRootKind::CacheData, "/var/cache/zccache")
        .publish_in(registry.path())
        .expect("publish manifest");

    let manifest = read_manifest(&path).expect("verify published manifest");
    assert_eq!(manifest.service_name, "zccache");
    assert_eq!(manifest.service_version, "1.11.20");
    assert_eq!(manifest.self_sha256.len(), 32);
    assert!(manifest.host.is_some());
    assert_eq!(scan_central(registry.path()).len(), 1);

    let mut tampered = manifest;
    tampered.self_sha256[0] ^= 1;
    fs::write(&path, tampered.encode_to_vec()).expect("write tampered manifest");
    assert!(matches!(
        read_manifest(&path),
        Err(ManifestError::Corruption)
    ));
}

#[test]
fn default_v1_registration_paths_keep_the_established_product_layout() {
    assert!(central_registry_dir().ends_with("running-process/manifests"));
    assert!(service_definition_dir().ends_with("running-process/services"));
}

#[test]
fn service_definition_builder_preserves_v1_file_bytes_and_loader_round_trip() {
    let root = tempfile::tempdir().expect("service tempdir");
    let definition = ServiceDefinitionBuilder::shared_broker("zccache", ABS_BINARY)
        .min_version("1.10.0")
        .allow_version("1.11.20")
        .label("team", "cache")
        .build()
        .expect("validate service definition");
    let expected = definition.encode_to_vec();
    let path =
        write_service_definition(root.path(), &definition).expect("write service definition");

    assert_eq!(
        path,
        root.path().join(format!("zccache.{SERVICE_DEF_EXTENSION}"))
    );
    assert_eq!(fs::read(&path).expect("read v1 file"), expected);
    assert_eq!(
        ServiceDefinitionLoader::new(root.path())
            .load("zccache")
            .expect("load v1 file"),
        definition
    );
}

#[test]
fn service_definition_validation_and_paths_remain_v1_compatible() {
    assert!(validate_service_name("zccache").is_ok());
    assert!(validate_service_name("Zccache").is_err());
    assert!(validate_version("1.2.3-rc.1").is_ok());
    assert!(validate_version("../../bad").is_err());

    let root = tempfile::tempdir().expect("service tempdir");
    let definition = ServiceDefinition {
        service_name: "zccache".into(),
        binary_path: ABS_BINARY.into(),
        isolation: BrokerIsolation::SharedBroker as i32,
        ..Default::default()
    };
    let path =
        write_service_definition(root.path(), &definition).expect("write validated definition");
    assert_eq!(
        path,
        service_definition_path(root.path(), "zccache").expect("v1 service path")
    );
}
