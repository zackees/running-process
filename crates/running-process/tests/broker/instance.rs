#![cfg(feature = "client")]

use running_process::broker::protocol::{BrokerIsolation, ServiceDefinition};
use running_process::broker::server::BrokerInstanceKey;

const USER_HASH: &str = "0123456789abcdef";

fn definition(isolation: BrokerIsolation) -> ServiceDefinition {
    ServiceDefinition {
        service_name: "zccache".into(),
        binary_path: "/usr/bin/zccache".into(),
        isolation: isolation as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: String::new(),
        min_version: "1.11.20".into(),
        version_allow_list: vec!["1.11.20".into()],
        labels: Default::default(),
    }
}

fn pick_path(path: running_process::broker::lifecycle::PipePath) -> String {
    path.windows
        .or_else(|| path.unix.map(|p| p.to_string_lossy().into_owned()))
        .unwrap()
}

#[test]
fn shared_service_uses_shared_instance() {
    let key = BrokerInstanceKey::from_service_definition(&definition(BrokerIsolation::SharedBroker))
        .unwrap();

    assert_eq!(key, BrokerInstanceKey::Shared);
    assert_eq!(key.id(), "shared");
    assert!(pick_path(key.pipe_path(USER_HASH).unwrap()).contains("shared"));
}

#[test]
fn private_service_uses_service_scoped_instance() {
    let key =
        BrokerInstanceKey::from_service_definition(&definition(BrokerIsolation::PrivateBroker))
            .unwrap();

    assert_eq!(
        key,
        BrokerInstanceKey::Private {
            service_name: "zccache".into()
        }
    );
    assert_eq!(key.id(), "private:zccache");
    assert!(pick_path(key.pipe_path(USER_HASH).unwrap()).contains("zccache"));
}

#[test]
fn explicit_service_uses_named_instance() {
    let mut definition = definition(BrokerIsolation::ExplicitInstance);
    definition.explicit_instance = "ci-trusted".into();
    let key = BrokerInstanceKey::from_service_definition(&definition).unwrap();

    assert_eq!(
        key,
        BrokerInstanceKey::Explicit {
            name: "ci-trusted".into()
        }
    );
    assert_eq!(key.id(), "explicit:ci-trusted");
    assert!(pick_path(key.pipe_path(USER_HASH).unwrap()).contains("ci-trusted"));
}

#[test]
fn explicit_service_requires_instance_name() {
    let err = BrokerInstanceKey::from_service_definition(&definition(BrokerIsolation::ExplicitInstance))
        .unwrap_err();

    assert!(err.to_string().contains("requires explicit_instance"));
}
