//! External-consumer source compatibility for protocol type extensions (#1149).

#![cfg(feature = "client")]

use running_process::broker::protocol_v2::SessionStart;
use running_process::EnvironmentPolicy;

#[test]
fn session_start_policy_builder_needs_no_extension_trait_import() {
    let start = SessionStart::from_current_process("rustc", ["--version"], "work")
        .with_environment_policy(EnvironmentPolicy::Inherit);

    assert_eq!(start.environment_policy, 1);
    assert!(!start.clear_inherited_env);
}
