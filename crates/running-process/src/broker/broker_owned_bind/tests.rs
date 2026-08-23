use super::*;

/// The launcher binds by default, and a falsy value opts out.
///
/// The escape hatch used to open for `0` alone; it now opens for every falsy
/// spelling, because a user who writes `=false` is plainly asking for the
/// fallback and silently keeping the default is the wrong way to be wrong.
#[test]
fn the_launcher_binds_by_default_and_a_falsy_value_opts_out() {
    let name = LAUNCHER_OPT_IN_ENV;
    assert!(with_launcher_opt_in(None, launcher_opt_in), "unset binds");
    for still_binds in ["1", "true", "yes", "on", "anything-else"] {
        assert!(
            with_launcher_opt_in(Some(still_binds), launcher_opt_in),
            "{still_binds:?} must leave broker-owned bind in place ({name})"
        );
    }
    for opts_out in ["0", "false", "no", "off", ""] {
        assert!(
            !with_launcher_opt_in(Some(opts_out), launcher_opt_in),
            "{opts_out:?} must fall back to spawn-then-probe ({name})"
        );
    }
}

fn with_launcher_opt_in<T>(value: Option<&str>, body: impl FnOnce() -> T) -> T {
    let previous = std::env::var_os(LAUNCHER_OPT_IN_ENV);
    match value {
        Some(value) => std::env::set_var(LAUNCHER_OPT_IN_ENV, value),
        None => std::env::remove_var(LAUNCHER_OPT_IN_ENV),
    }
    let outcome = body();
    match previous {
        Some(previous) => std::env::set_var(LAUNCHER_OPT_IN_ENV, previous),
        None => std::env::remove_var(LAUNCHER_OPT_IN_ENV),
    }
    outcome
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
