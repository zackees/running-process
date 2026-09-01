use running_process::daemon_registration_v2::{
    service_definition_path_v2, BrokerIsolation, ServiceDefinitionBuilder,
};

fn main() {
    let definition = ServiceDefinitionBuilder::shared_broker("zccache", "/usr/bin/zccache")
        .version_allow_list(["1.2.3", "1.2.4"])
        .label("package", "zccache")
        .build();
    assert_eq!(definition.isolation, BrokerIsolation::SharedBroker as i32);
    let _path = service_definition_path_v2(std::path::Path::new("/services"), "zccache")
        .expect("valid v2 service definition path");
}
