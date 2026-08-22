//! Process environment baselines.
//!
//! Reconstructing the logged-in user's environment is a host mechanic and
//! lives in [`crate::platform::host`]: Windows builds it from machine and user
//! settings, Unix rebuilds it from the passwd entry because no Unix API
//! reconstructs a login environment. What this module owns is the *policy* --
//! which baseline a spawn starts from, and how explicit entries are layered on
//! top of it.

use std::ffi::OsString;
use std::io;

/// Return the logged-in user's baseline environment.
///
/// This is the environment a fresh login would have, not a copy of this
/// process's: variables that exist only here do not appear in it. See
/// [`crate::platform::host::login_environment`] for what each host builds it
/// from.
pub fn user_baseline_environment() -> io::Result<Vec<(OsString, OsString)>> {
    crate::platform::host::login_environment()
}

/// Return a `CreateProcessW`-compatible Unicode user environment block.
///
/// The returned buffer is sorted and double-NUL terminated by Windows. It is
/// useful to callers that own a manual `CreateProcessW` path, and is exported
/// only here because no other host has an API that consumes this shape.
#[cfg(windows)]
pub fn user_baseline_environment_block() -> io::Result<Vec<u16>> {
    crate::platform::host::login_environment_block()
}

/// Materialize a string environment for backends whose native API accepts
/// either an inherited environment (`None`) or one complete replacement
/// block (`Some`). Ordered explicit entries are applied after the selected
/// base and win ties, matching how this host compares variable names.
#[cfg(any(feature = "daemon", test))]
pub(crate) fn materialize_environment(
    policy: crate::EnvironmentPolicy,
    explicit: &[(String, String)],
) -> io::Result<Option<Vec<(String, String)>>> {
    if policy == crate::EnvironmentPolicy::Inherit && explicit.is_empty() {
        return Ok(None);
    }

    let mut output: Vec<(String, String)> = match policy {
        crate::EnvironmentPolicy::Inherit => std::env::vars().collect(),
        crate::EnvironmentPolicy::UserBaseline => user_baseline_environment()?
            .into_iter()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect(),
        crate::EnvironmentPolicy::Clear => Vec::new(),
        crate::EnvironmentPolicy::Auto => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Auto environment policy must be resolved before materialization",
            ));
        }
    };

    for (key, value) in explicit {
        // An explicit `Path=` must replace an inherited `PATH` on a host where
        // those name the same variable, and must not on one where they do not.
        // Asking the host settles it; guessing from the current OS is how the
        // two spellings end up both present and one of them ignored.
        let existing = output
            .iter_mut()
            .find(|(candidate, _)| environment_keys_match(candidate, key));
        if let Some((existing_key, existing_value)) = existing {
            *existing_key = key.clone();
            *existing_value = value.clone();
        } else {
            output.push((key.clone(), value.clone()));
        }
    }
    Ok(Some(output))
}

/// Whether two environment variable names refer to the same variable here.
#[cfg(any(feature = "daemon", test))]
fn environment_keys_match(left: &str, right: &str) -> bool {
    if crate::platform::host::environment_keys_are_case_insensitive() {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

#[cfg(test)]
mod materialize_tests {
    use super::*;

    #[test]
    fn clear_uses_only_explicit_entries() {
        let env = materialize_environment(
            crate::EnvironmentPolicy::Clear,
            &[("CLIENT_ONLY".into(), "forwarded".into())],
        )
        .unwrap()
        .unwrap();
        assert_eq!(env, vec![("CLIENT_ONLY".into(), "forwarded".into())]);
    }

    #[test]
    fn empty_inherit_uses_native_inheritance() {
        assert_eq!(
            materialize_environment(crate::EnvironmentPolicy::Inherit, &[]).unwrap(),
            None
        );
    }

    #[test]
    fn unresolved_auto_is_rejected() {
        assert!(materialize_environment(crate::EnvironmentPolicy::Auto, &[]).is_err());
    }

    /// Explicit entries replace an existing variable exactly when this host
    /// says the two names are the same variable -- so the assertion is written
    /// against that answer rather than against one host's rule.
    #[test]
    fn explicit_entries_replace_by_this_hosts_name_comparison() {
        let env = materialize_environment(
            crate::EnvironmentPolicy::Clear,
            &[
                ("ExampleVar".into(), "first".into()),
                ("EXAMPLEVAR".into(), "second".into()),
            ],
        )
        .unwrap()
        .unwrap();

        if crate::platform::host::environment_keys_are_case_insensitive() {
            assert_eq!(
                env,
                vec![("EXAMPLEVAR".to_string(), "second".to_string())],
                "one variable, last spelling and value win"
            );
        } else {
            assert_eq!(
                env,
                vec![
                    ("ExampleVar".to_string(), "first".to_string()),
                    ("EXAMPLEVAR".to_string(), "second".to_string()),
                ],
                "two distinct variables"
            );
        }
    }

    /// The baseline is the user's, not this process's.
    #[test]
    fn user_baseline_excludes_process_only_variables() {
        std::env::set_var("RUNNING_PROCESS_MATERIALIZE_CANARY", "1");
        let env = materialize_environment(crate::EnvironmentPolicy::UserBaseline, &[]).unwrap();
        std::env::remove_var("RUNNING_PROCESS_MATERIALIZE_CANARY");
        let env = env.expect("UserBaseline always materializes a block");
        assert!(
            !env.iter()
                .any(|(key, _)| key == "RUNNING_PROCESS_MATERIALIZE_CANARY"),
            "a process-local variable must not reach the user baseline"
        );
    }
}
