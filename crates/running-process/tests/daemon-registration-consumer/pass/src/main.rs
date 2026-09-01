use running_process::daemon_registration::{
    builders::ServiceDefinitionBuilder,
    protocol::BrokerIsolation,
    validation::validate_version,
};

fn main() {
    // The fixture only type-checks the public API; the integration E2E covers
    // validated platform paths at runtime on its executing host.
    let binary = "/usr/local/bin/zccache";

    let definition = ServiceDefinitionBuilder::shared_broker("zccache", binary)
        .build()
        .expect("validated v1 service definition");
    assert_eq!(definition.isolation, BrokerIsolation::SharedBroker as i32);
    validate_version("1.2.3").expect("v1 version");
}
