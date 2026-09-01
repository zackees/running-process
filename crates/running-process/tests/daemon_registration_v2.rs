#![cfg(feature = "daemon-registration-v2")]

//! No-default public contract for the frozen v2 service-definition writer.

use std::fs;
use std::path::Path;
use std::sync::Mutex;

use prost::Message;
use running_process::daemon_registration_v2::{
    service_definition_dir_v2, service_definition_path_v2, write_service_definition_v2,
    BrokerIsolation, ServiceDefinition, ServiceDefinitionBuilder,
};

#[cfg(windows)]
const ABS_BINARY: &str = "C:\\tools\\zccache.exe";
#[cfg(not(windows))]
const ABS_BINARY: &str = "/usr/local/bin/zccache";

static SERVICE_DEF_ENV_LOCK: Mutex<()> = Mutex::new(());

fn with_service_definition_root<T>(root: Option<&Path>, body: impl FnOnce() -> T) -> T {
    let _lock = SERVICE_DEF_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let name = "RUNNING_PROCESS_SERVICE_DEF_DIR";
    let previous = std::env::var_os(name);
    match root {
        Some(root) => std::env::set_var(name, root),
        None => std::env::remove_var(name),
    }
    let outcome = body();
    match previous {
        Some(previous) => std::env::set_var(name, previous),
        None => std::env::remove_var(name),
    }
    outcome
}

#[test]
fn default_and_env_selected_v2_roots_keep_the_established_layout() {
    with_service_definition_root(None, || {
        assert!(service_definition_dir_v2().ends_with("running-process/services"));
    });

    let override_root = tempfile::tempdir().expect("override root");
    with_service_definition_root(Some(override_root.path()), || {
        assert_eq!(service_definition_dir_v2(), override_root.path());
    });
}

#[test]
fn v2_writer_preserves_semantic_fields_and_optional_http_absence() {
    let root = tempfile::tempdir().expect("service root");
    let definition = ServiceDefinitionBuilder::shared_broker("zccache", ABS_BINARY)
        .per_version_binary_dir(
            Path::new(ABS_BINARY)
                .parent()
                .expect("absolute binary has parent")
                .display()
                .to_string(),
        )
        .min_version("1.2.3")
        .version_allow_list(["1.2.3", "1.2.4"])
        .label("vendor", "zackees")
        .label("package", "zccache")
        .build();
    let path = write_service_definition_v2(root.path(), &definition).expect("write v2 record");

    assert_eq!(
        path,
        service_definition_path_v2(root.path(), "zccache").expect("v2 path")
    );
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some("zccache.servicedef.v2")
    );
    let decoded = ServiceDefinition::decode(fs::read(&path).expect("read record").as_slice())
        .expect("decode v2 record");
    assert_eq!(decoded.service_name, "zccache");
    assert_eq!(decoded.binary_path, ABS_BINARY);
    assert_eq!(decoded.isolation, BrokerIsolation::SharedBroker as i32);
    assert_eq!(decoded.version_allow_list, vec!["1.2.3", "1.2.4"]);
    assert_eq!(decoded.labels.get("vendor"), Some(&"zackees".to_owned()));
    assert_eq!(decoded.labels.get("package"), Some(&"zccache".to_owned()));
    assert!(decoded.http_server.is_none());
}

#[test]
fn invalid_v2_service_name_does_not_create_a_record() {
    let root = tempfile::tempdir().expect("service root");
    let definition = ServiceDefinition {
        service_name: "Zccache".to_owned(),
        ..Default::default()
    };
    assert!(write_service_definition_v2(root.path(), &definition).is_err());
    assert!(!root.path().join("Zccache.servicedef.v2").exists());
}

#[test]
fn v2_writer_creates_an_owner_private_root() {
    let parent = tempfile::tempdir().expect("parent");
    let root = parent.path().join("registration");
    let path = ServiceDefinitionBuilder::shared_broker("zccache", ABS_BINARY)
        .install_in(&root)
        .expect("write v2 record");
    assert!(path.exists());
    assert!(root.is_dir());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(&root)
                .expect("root metadata")
                .permissions()
                .mode()
                & 0o077,
            0,
            "service-definition root must not grant group/world permissions"
        );
    }
}

#[cfg(feature = "daemon-registration")]
#[test]
fn v1_and_v2_service_definitions_coexist_under_the_same_root() {
    use running_process::daemon_registration::{
        builders::ServiceDefinitionBuilder as V1ServiceDefinitionBuilder,
        service_def_loader::write_service_definition,
    };

    let root = tempfile::tempdir().expect("service root");
    let v1 = V1ServiceDefinitionBuilder::shared_broker("zccache", ABS_BINARY)
        .build()
        .expect("valid v1 definition");
    let v1_path = write_service_definition(root.path(), &v1).expect("write v1 record");
    let v2_path = ServiceDefinitionBuilder::shared_broker("zccache", ABS_BINARY)
        .install_in(root.path())
        .expect("write v2 record");

    assert_eq!(
        v1_path.file_name().and_then(|name| name.to_str()),
        Some("zccache.servicedef")
    );
    assert_eq!(
        v2_path.file_name().and_then(|name| name.to_str()),
        Some("zccache.servicedef.v2")
    );
    assert!(v1_path.exists());
    assert!(v2_path.exists());
    assert_eq!(
        running_process::daemon_registration_v2::SERVICE_DEF_V2_EXTENSION,
        "servicedef.v2"
    );
}
