//! Canonical, dependency-light policy defaults for #1202 and kernal-api#189.
use running_process::{SpawnLifetime, SpawnMode, SpawnOptions};

#[test]
fn default_spawn_policy_is_inherited_and_handle_bound() {
    let options = SpawnOptions::default();
    assert_eq!(SpawnMode::default(), SpawnMode::Inherited);
    assert_eq!(options.mode, SpawnMode::Inherited);
    assert_eq!(options.lifetime, SpawnLifetime::KillOnDrop);
    assert!(options.backend.is_none());
}

#[test]
fn resource_mode_and_detached_lifetime_are_separate() {
    let options = SpawnOptions {
        lifetime: SpawnLifetime::Detached,
        ..SpawnOptions::default()
    };
    assert_eq!(options.mode, SpawnMode::Inherited);
    let independent = SpawnOptions {
        mode: SpawnMode::Independent,
        ..options
    };
    assert_eq!(independent.lifetime, SpawnLifetime::Detached);
}

#[cfg(all(target_os = "linux", feature = "independent-spawn"))]
#[test]
fn inherited_dispatch_needs_no_scheduler_and_keeps_placement() {
    use running_process::{
        independent_spawn::{LaunchSpec, Readiness},
        spawn_with_options,
    };
    use std::{fs, sync::atomic::AtomicBool, time::Duration};
    let spec = LaunchSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "exec sleep 30".into()],
        cwd: "/".into(),
        environment: vec![("PATH".into(), std::env::var_os("PATH").unwrap())],
        stdout: None,
        stderr: None,
        readiness: Readiness::ProcessStarted,
    };
    let mut child =
        spawn_with_options(&spec, &SpawnOptions::default(), &AtomicBool::new(false)).unwrap();
    assert_eq!(child.actual_mode(), SpawnMode::Inherited);
    assert_eq!(
        fs::read_to_string("/proc/self/cgroup").unwrap(),
        fs::read_to_string(format!("/proc/{}/cgroup", child.id())).unwrap()
    );
    child.stop(Duration::from_secs(2)).unwrap();
}

#[cfg(feature = "independent-spawn")]
#[test]
fn authority_selection_is_explicit_and_cancellation_precedes_side_effects() {
    use running_process::{
        independent_spawn::{LaunchSpec, Readiness},
        spawn_with_options, IndependentBackend,
    };
    use std::{io::ErrorKind, sync::atomic::AtomicBool, time::Duration};
    let spec = LaunchSpec {
        program: "unused".into(),
        args: vec![],
        cwd: ".".into(),
        environment: vec![],
        stdout: None,
        stderr: None,
        readiness: Readiness::ProcessStarted,
    };
    let options = SpawnOptions {
        mode: SpawnMode::Independent,
        ..SpawnOptions::default()
    };
    assert_eq!(
        spawn_with_options(&spec, &options, &AtomicBool::new(false))
            .err()
            .unwrap()
            .kind(),
        ErrorKind::Unsupported
    );
    assert_eq!(
        spawn_with_options(&spec, &options, &AtomicBool::new(true))
            .err()
            .unwrap()
            .kind(),
        ErrorKind::Interrupted
    );
    let options = SpawnOptions {
        backend: Some(IndependentBackend::NativeScheduler {
            launcher: "unused".into(),
        }),
        ..SpawnOptions::default()
    };
    assert_eq!(
        spawn_with_options(&spec, &options, &AtomicBool::new(false))
            .err()
            .unwrap()
            .kind(),
        ErrorKind::InvalidInput
    );
    let options = SpawnOptions {
        timeout: Duration::ZERO,
        ..SpawnOptions::default()
    };
    assert_eq!(
        spawn_with_options(&spec, &options, &AtomicBool::new(false))
            .err()
            .unwrap()
            .kind(),
        ErrorKind::InvalidInput
    );
}

#[cfg(all(target_os = "linux", feature = "independent-spawn"))]
#[test]
fn inherited_exit_observation_retains_identity_until_handle_cleanup() {
    use running_process::{
        independent_spawn::{LaunchSpec, Readiness},
        spawn_with_options,
    };
    use std::{fs, sync::atomic::AtomicBool, time::Duration};
    let spec = LaunchSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "exit 7".into()],
        cwd: "/".into(),
        environment: vec![],
        stdout: None,
        stderr: None,
        readiness: Readiness::ProcessStarted,
    };
    let cancelled = AtomicBool::new(false);
    let mut child = spawn_with_options(&spec, &SpawnOptions::default(), &cancelled).unwrap();
    let pid = child.id();
    assert_eq!(
        child.wait(Duration::from_secs(2), &cancelled).unwrap().code,
        Some(7)
    );
    // A numeric process group cannot be controlled safely once its leader's
    // identity has been released. Observing exit must keep the owned leader
    // waitable until teardown has finished using that group identity.
    let retained = fs::read_to_string(format!("/proc/{pid}/stat"));
    child.stop(Duration::from_secs(2)).unwrap();
    drop(child);
    assert!(
        retained.is_ok(),
        "exit observation released the group leader identity"
    );
    assert_eq!(
        retained
            .unwrap()
            .rsplit_once(") ")
            .unwrap()
            .1
            .split_whitespace()
            .next(),
        Some("Z")
    );
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
}

#[cfg(all(target_os = "linux", feature = "independent-spawn"))]
#[test]
fn failed_inherited_detached_readiness_rolls_back_before_returning() {
    use running_process::{
        independent_spawn::{LaunchSpec, Readiness},
        spawn_with_options,
    };
    use std::{fs, path::Path, sync::atomic::AtomicBool, time::Duration};
    let directory = tempfile::tempdir().unwrap();
    let pidfile = directory.path().join("pid");
    let descendant_file = directory.path().join("descendant");
    let spec = LaunchSpec {
        program: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "printf '%s' $$ > \"$1\"; sleep 4 & printf '%s' $! > \"$2\"; wait".into(),
            "fixture".into(),
            pidfile.as_os_str().to_owned(),
            descendant_file.as_os_str().to_owned(),
        ],
        cwd: directory.path().as_os_str().to_owned(),
        environment: vec![("PATH".into(), std::env::var_os("PATH").unwrap())],
        stdout: None,
        stderr: None,
        readiness: Readiness::File {
            path: directory.path().join("never-ready").into_os_string(),
            value: b"ready".to_vec(),
        },
    };
    let options = SpawnOptions {
        lifetime: SpawnLifetime::Detached,
        timeout: Duration::from_millis(200),
        ..SpawnOptions::default()
    };
    assert_eq!(
        spawn_with_options(&spec, &options, &AtomicBool::new(false))
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::TimedOut
    );
    let pid: u32 = fs::read_to_string(pidfile).unwrap().parse().unwrap();
    let descendant: u32 = fs::read_to_string(descendant_file)
        .unwrap()
        .parse()
        .unwrap();
    let descendant_alive = || {
        fs::read_to_string(format!("/proc/{descendant}/stat"))
            .ok()
            .is_some_and(|stat| {
                stat.rsplit_once(')').unwrap().1.split_whitespace().next() != Some("Z")
            })
    };
    let survived = descendant_alive();
    // A finite child also cleans up the intentionally failing RED run.
    let cleanup = std::time::Instant::now() + Duration::from_secs(5);
    while descendant_alive() && std::time::Instant::now() < cleanup {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !survived,
        "failed detached launch left a running descendant"
    );
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "failed detached launch survived rollback"
    );
}

#[cfg(all(target_os = "linux", feature = "independent-spawn"))]
#[test]
#[ignore = "requires a running systemd user manager and cgroup v2"]
fn independent_dispatch_reports_verified_mode_and_kills_on_drop() {
    use running_process::{
        independent_spawn::{LaunchSpec, Readiness},
        spawn_with_options, IndependentBackend,
    };
    use std::{fs, path::Path, sync::atomic::AtomicBool};
    let spec = LaunchSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "exec sleep 30".into()],
        cwd: "/".into(),
        environment: vec![("PATH".into(), std::env::var_os("PATH").unwrap())],
        stdout: None,
        stderr: None,
        readiness: Readiness::ProcessStarted,
    };
    let options = SpawnOptions {
        mode: SpawnMode::Independent,
        backend: Some(IndependentBackend::NativeScheduler {
            launcher: env!("CARGO_BIN_EXE_running-process-launcher").into(),
        }),
        ..SpawnOptions::default()
    };
    let child = spawn_with_options(&spec, &options, &AtomicBool::new(false)).unwrap();
    assert_eq!(child.actual_mode(), SpawnMode::Independent);
    let pid = child.id();
    assert_ne!(
        fs::read_to_string("/proc/self/cgroup").unwrap(),
        fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap()
    );
    drop(child);
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "handle-bound independent target survived drop"
    );
}
