use super::*;

#[test]
fn the_launcher_binds_by_default_and_only_an_explicit_zero_opts_out() {
    use std::ffi::OsString;

    assert!(opted_in(None));
    assert!(opted_in(Some(OsString::from("1"))));
    assert!(opted_in(Some(OsString::from("true"))));
    assert!(!opted_in(Some(OsString::from("0"))));
}

#[test]
fn environment_contract_is_project_namespaced() {
    assert!(LAUNCHER_OPT_IN_ENV.starts_with("RUNNING_PROCESS_"));
    assert!(INHERITED_LISTENER_FD_ENV.starts_with("RUNNING_PROCESS_"));
}

#[test]
fn no_environment_value_means_no_inherited_listener() {
    if std::env::var_os(INHERITED_LISTENER_FD_ENV).is_some() {
        eprintln!("skipping: inherited-listener environment is set");
        return;
    }
    assert!(recover_from_env()
        .expect("absence is not an error")
        .is_none());
}

#[test]
fn support_query_matches_the_platform_capability() {
    assert_eq!(support().is_supported(), InheritableListener::supported());
}
