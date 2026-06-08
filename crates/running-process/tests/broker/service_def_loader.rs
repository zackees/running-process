#![cfg(feature = "client")]

use std::fs;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use prost::Message;
use running_process::broker::protocol::{BrokerIsolation, ServiceDefinition};
use running_process::broker::server::{
    ensure_service_definition_dir, service_definition_path, ServiceDefinitionError,
    ServiceDefinitionLoader,
};

fn absolute_paths() -> (String, String) {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().to_path_buf();
    (
        exe.to_string_lossy().into_owned(),
        dir.to_string_lossy().into_owned(),
    )
}

fn service_definition() -> ServiceDefinition {
    let (binary_path, per_version_binary_dir) = absolute_paths();
    ServiceDefinition {
        service_name: "zccache".into(),
        binary_path,
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir,
        min_version: "1.10.0".into(),
        version_allow_list: vec!["1.11.20".into()],
        labels: Default::default(),
    }
}

fn write_definition_for(root: &Path, service_name: &str, definition: &ServiceDefinition) {
    let path = service_definition_path(root, service_name).unwrap();
    fs::write(path, definition.encode_to_vec()).unwrap();
}

#[test]
fn loader_reads_valid_service_definition() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");
    ensure_service_definition_dir(&root).unwrap();
    write_definition_for(&root, "zccache", &service_definition());

    let loaded = ServiceDefinitionLoader::new(&root).load("zccache").unwrap();

    assert_eq!(loaded.service_name, "zccache");
    assert_eq!(loaded.min_version, "1.10.0");
    assert_eq!(loaded.version_allow_list, vec!["1.11.20"]);
}

#[test]
fn lookup_or_reload_rereads_changed_file() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");
    ensure_service_definition_dir(&root).unwrap();

    let mut definition = service_definition();
    write_definition_for(&root, "zccache", &definition);
    assert_eq!(
        ServiceDefinitionLoader::new(&root)
            .lookup_or_reload("zccache")
            .unwrap()
            .min_version,
        "1.10.0"
    );

    definition.min_version = "1.11.0".into();
    write_definition_for(&root, "zccache", &definition);
    assert_eq!(
        ServiceDefinitionLoader::new(&root)
            .lookup_or_reload("zccache")
            .unwrap()
            .min_version,
        "1.11.0"
    );
}

#[test]
fn loader_rejects_service_name_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");
    ensure_service_definition_dir(&root).unwrap();

    let mut definition = service_definition();
    definition.service_name = "clud".into();
    write_definition_for(&root, "zccache", &definition);

    let err = ServiceDefinitionLoader::new(&root)
        .load("zccache")
        .unwrap_err();
    match err {
        ServiceDefinitionError::ServiceNameMismatch { requested, actual } => {
            assert_eq!(requested, "zccache");
            assert_eq!(actual, "clud");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn loader_rejects_invalid_version_allow_list() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");
    ensure_service_definition_dir(&root).unwrap();

    let mut definition = service_definition();
    definition.version_allow_list = vec!["v1.11.20".into()];
    write_definition_for(&root, "zccache", &definition);

    let err = ServiceDefinitionLoader::new(&root)
        .load("zccache")
        .unwrap_err();
    assert!(matches!(err, ServiceDefinitionError::InvalidName(_)));
}

#[test]
fn loader_rejects_invalid_explicit_instance_policy() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");
    ensure_service_definition_dir(&root).unwrap();

    let mut definition = service_definition();
    definition.isolation = BrokerIsolation::ExplicitInstance as i32;
    definition.explicit_instance.clear();
    write_definition_for(&root, "zccache", &definition);

    let err = ServiceDefinitionLoader::new(&root)
        .load("zccache")
        .unwrap_err();
    assert!(matches!(
        err,
        ServiceDefinitionError::InvalidIsolation { .. }
    ));
}

#[cfg(unix)]
#[test]
fn loader_rejects_group_or_other_accessible_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");
    fs::create_dir_all(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
    write_definition_for(&root, "zccache", &service_definition());

    let err = ServiceDefinitionLoader::new(&root)
        .load("zccache")
        .unwrap_err();
    match err {
        ServiceDefinitionError::InsecureDirectory(path) => assert_eq!(path, root),
        other => panic!("unexpected error: {other:?}"),
    }
}
