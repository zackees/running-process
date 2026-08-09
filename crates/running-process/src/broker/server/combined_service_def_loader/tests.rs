use super::*;
use crate::broker::protocol::{BrokerIsolation as V1Isolation, ServiceDefinition as V1Def};
use crate::broker::protocol_v2::{
    HttpServerCapability, ServiceDefinition as V2Def, ServiceDefinitionBuilder,
};
use crate::broker::server::service_def_loader::{
    ensure_service_definition_dir, write_service_definition,
};
use std::collections::HashMap;
use tempfile::{tempdir, TempDir};

/// An OS-absolute backend path. The v1 loader validates `binary_path` with
/// `Path::is_absolute()`, which is drive-rooted on Windows and `/`-rooted on
/// Unix, so tests that write v1 files must use a path absolute for the host.
#[cfg(windows)]
fn abs(name: &str) -> String {
    format!("C:\\bin\\{name}.exe")
}
#[cfg(not(windows))]
fn abs(name: &str) -> String {
    format!("/usr/bin/{name}")
}

/// Create a freshly-secured `services` subdirectory. A brand-new dir gets the
/// current-user-only ACL (Windows) / 0700 (Unix) the v1 loader requires;
/// retrofitting an existing tempdir's permissions is not reliable cross-platform
/// (this mirrors `hello_router`'s test fixture).
fn secure_service_root(dir: &TempDir) -> PathBuf {
    let root = dir.path().join("services");
    ensure_service_definition_dir(&root).expect("secure service dir");
    root
}

fn install_v2(root: &Path, name: &str, binary: &str) {
    ServiceDefinitionBuilder::shared_broker(name, binary)
        .min_version("1.0.0")
        .label("env", "test")
        .install_in(root)
        .expect("install v2 servicedef");
}

fn write_v1(root: &Path, name: &str, binary: &str) {
    let def = V1Def {
        service_name: name.to_string(),
        binary_path: binary.to_string(),
        isolation: V1Isolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: String::new(),
        min_version: "1.0.0".to_string(),
        version_allow_list: Vec::new(),
        labels: HashMap::new(),
    };
    write_service_definition(root, &def).expect("write v1 servicedef");
}

#[test]
fn loads_v2_file_and_converts_to_v1() {
    let dir = tempdir().expect("tempdir");
    let root = secure_service_root(&dir);
    let bin = abs("zccache-daemon");
    install_v2(&root, "zccache", &bin);

    let loaded = CombinedServiceDefinitionLoader::new(root.clone())
        .lookup_or_reload("zccache")
        .expect("v2 servicedef must load through the combined loader");

    assert_eq!(loaded.service_name, "zccache");
    assert_eq!(loaded.binary_path, bin);
    assert_eq!(loaded.isolation, V1Isolation::SharedBroker as i32);
    assert_eq!(loaded.min_version, "1.0.0");
    assert_eq!(loaded.labels.get("env").map(String::as_str), Some("test"));
}

#[test]
fn falls_back_to_v1_when_no_v2_file() {
    let dir = tempdir().expect("tempdir");
    let root = secure_service_root(&dir);
    let bin = abs("zccache-daemon");
    write_v1(&root, "zccache", &bin);

    let loaded = CombinedServiceDefinitionLoader::new(root.clone())
        .lookup_or_reload("zccache")
        .expect("v1 servicedef must load via fallback when no v2 file exists");

    assert_eq!(loaded.service_name, "zccache");
    assert_eq!(loaded.binary_path, bin);
    assert_eq!(loaded.isolation, V1Isolation::SharedBroker as i32);
}

#[test]
fn prefers_v2_over_v1_when_both_present() {
    let dir = tempdir().expect("tempdir");
    let root = secure_service_root(&dir);
    let v1_bin = abs("v1-daemon");
    let v2_bin = abs("v2-daemon");
    write_v1(&root, "zccache", &v1_bin);
    install_v2(&root, "zccache", &v2_bin);

    let loaded = CombinedServiceDefinitionLoader::new(root.clone())
        .lookup_or_reload("zccache")
        .expect("load");

    assert_eq!(
        loaded.binary_path, v2_bin,
        "the v2 file must win when both extensions are present"
    );
}

#[test]
fn missing_both_returns_io_not_found() {
    let dir = tempdir().expect("tempdir");
    let root = secure_service_root(&dir);
    let err = CombinedServiceDefinitionLoader::new(root.clone())
        .lookup_or_reload("no-such-service")
        .expect_err("missing both files must error");
    assert!(
        matches!(err, ServiceDefinitionError::Io(ref io) if io.kind() == std::io::ErrorKind::NotFound),
        "missing both → Io(NotFound), got: {err:?}"
    );
}

#[test]
fn corrupt_v2_is_surfaced_not_masked_by_v1_fallback() {
    let dir = tempdir().expect("tempdir");
    // A valid v1 file exists, but the v2 file is corrupt. The corrupt v2 must
    // surface as a decode error rather than silently falling back to v1 —
    // a broken install should be loud, not quietly served from a stale format.
    let root = secure_service_root(&dir);
    write_v1(&root, "zccache", &abs("zccache-daemon"));
    std::fs::write(root.join("zccache.servicedef.v2"), b"not a protobuf")
        .expect("write corrupt v2");

    let err = CombinedServiceDefinitionLoader::new(root.clone())
        .lookup_or_reload("zccache")
        .expect_err("a corrupt v2 file must not be masked by v1 fallback");
    assert!(
        matches!(err, ServiceDefinitionError::Decode(_)),
        "corrupt v2 → Decode error, got: {err:?}"
    );
}

#[test]
fn conversion_carries_shared_fields_and_drops_http_server() {
    let mut labels = HashMap::new();
    labels.insert("team".to_string(), "infra".to_string());
    let v2 = V2Def {
        service_name: "zccache".to_string(),
        binary_path: abs("zccache-daemon"),
        isolation: V1Isolation::ExplicitInstance as i32,
        explicit_instance: "ci-trusted".to_string(),
        per_version_binary_dir: "/opt/zccache/bin".to_string(),
        min_version: "2.1.0".to_string(),
        version_allow_list: vec!["2.1.0".to_string(), "2.2.0".to_string()],
        labels: labels.clone(),
        // v2-only capability that has no v1 equivalent and must be dropped.
        http_server: Some(HttpServerCapability {
            bind_addr: "127.0.0.1".to_string(),
            health_path: "/health".to_string(),
            display_name: "zccache".to_string(),
        }),
    };

    let v1 = service_definition_v2_to_v1(v2);

    assert_eq!(v1.service_name, "zccache");
    assert_eq!(v1.binary_path, abs("zccache-daemon"));
    assert_eq!(v1.isolation, V1Isolation::ExplicitInstance as i32);
    assert_eq!(v1.explicit_instance, "ci-trusted");
    assert_eq!(v1.per_version_binary_dir, "/opt/zccache/bin");
    assert_eq!(v1.min_version, "2.1.0");
    assert_eq!(v1.version_allow_list, vec!["2.1.0", "2.2.0"]);
    assert_eq!(v1.labels, labels);
}
