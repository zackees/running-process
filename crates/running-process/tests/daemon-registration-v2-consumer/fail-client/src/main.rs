// The direct writer feature must not compile the broad client/broker module.
use running_process::broker::protocol_v2::ServiceDefinitionBuilder;

fn main() {
    let _ = ServiceDefinitionBuilder::shared_broker("zccache", "/usr/bin/zccache");
}
