//! Real containment regression for #1202; detached lifetime is not placement.
#![cfg(target_os = "linux")]

use std::fs;
use std::process::Command;

fn detached_placement() -> (String, String) {
    let caller = fs::read_to_string("/proc/self/cgroup").expect("caller cgroups");
    let mut command = Command::new("sleep");
    command.arg("60");
    let mut child = running_process::spawn::spawn_daemon(&mut command).expect("detach");
    let placement = fs::read_to_string(format!("/proc/{}/cgroup", child.id()));
    // Always clean up before assertions, including the intentionally failing repro.
    child.kill().expect("stop fixture");
    child.wait().expect("reap fixture");
    (caller, placement.expect("daemon cgroups"))
}

#[test]
fn detached_lifetime_preserves_inherited_cgroups() {
    let (caller, child) = detached_placement();
    assert_eq!(caller, child);
}

#[test]
#[ignore = "RED repro for #1202: legacy detached spawn cannot provide independence"]
fn legacy_detach_does_not_satisfy_independent_placement() {
    let (caller, child) = detached_placement();
    let unified = caller.lines().find_map(|line| line.strip_prefix("0::"));
    if let Some(path) = unified {
        let directory = format!("/sys/fs/cgroup{path}");
        for counter in ["memory.max", "memory.current"] {
            let value = fs::read_to_string(format!("{directory}/{counter}"))
                .expect("cgroup memory accounting");
            eprintln!("{directory}/{counter} = {}", value.trim());
        }
    }
    assert_ne!(
        caller, child,
        "detached daemon is still charged to caller cgroups"
    );
}
