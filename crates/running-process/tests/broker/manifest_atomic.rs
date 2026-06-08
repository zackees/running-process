#![cfg(feature = "client")]

use running_process::broker::manifest;
use running_process::broker::protocol::CacheManifest;

fn manifest_for(version: &str) -> CacheManifest {
    CacheManifest {
        manifest_schema_version: 1,
        media_type: manifest::CACHE_MANIFEST_MEDIA_TYPE.to_string(),
        self_sha256: Vec::new(),
        host: Some(running_process::broker::host_identity::current()),
        current_operation: None,
        valid_until_unix_ms: 0,
        service_name: "zccache".to_string(),
        service_version: version.to_string(),
        broker_envelope_version: "v1".to_string(),
        created_at_unix_ms: 1,
        last_active_unix_ms: 2,
        roots: Vec::new(),
        current_daemon: None,
        cleanup_policy: None,
        broker_instance: "shared".to_string(),
        depends_on: Vec::new(),
        provides: Vec::new(),
        observability: None,
        bundle_id: String::new(),
    }
}

#[test]
fn repeated_write_never_leaves_torn_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    manifest::write_to_root(&root, &manifest_for("1.2.3")).unwrap();
    manifest::write_to_root(&root, &manifest_for("1.2.4")).unwrap();

    let read = manifest::read_manifest(&root.join(manifest::ROOT_MANIFEST_FILE)).unwrap();
    assert_eq!(read.service_version, "1.2.4");
    assert_eq!(read.self_sha256.len(), 32);
}
