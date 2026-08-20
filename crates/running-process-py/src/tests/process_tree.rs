use std::process::{Child, Command};
use std::time::{Duration, Instant};
use sysinfo::System;

use crate::helpers::{descendant_pids, system_pid};
use crate::process_tree::{
    kill_process_tree_impl, native_get_process_tree_info, native_launch_detached,
    terminate_process_tree_impl,
};
use crate::registry::{process_created_at, same_process_identity};

// ── kill_process_tree_impl tests ──

#[test]
fn kill_process_tree_nonexistent_pid_no_panic() {
    // Should not panic when given a PID that doesn't exist
    kill_process_tree_impl(99999999, 0.1);
}

// ── descendant_pids tests ──

#[test]
fn descendant_pids_returns_empty_for_unknown_pid() {
    let system = System::new();
    let pid = system_pid(99999999);
    let descendants = descendant_pids(&system, pid);
    assert!(descendants.is_empty());
}

// ── same_process_identity tests ──

#[test]
fn same_process_identity_nonexistent_pid() {
    assert!(!same_process_identity(99999999, 0.0, 1.0));
}

// ── Iteration 3: Utility function tests ──

#[test]
fn kill_process_tree_nonexistent_pid_is_noop() {
    kill_process_tree_impl(999999, 0.5);
}

#[test]
fn terminate_process_tree_nonexistent_pid_is_verified() {
    assert!(terminate_process_tree_impl(999999, 0.5));
}

#[test]
fn get_process_tree_info_current_pid() {
    let pid = std::process::id();
    let info = native_get_process_tree_info(pid);
    assert!(info.contains(&format!("{}", pid)));
}

#[test]
fn get_process_tree_info_nonexistent_pid() {
    let info = native_get_process_tree_info(999999);
    assert!(info.contains("Could not get process info"));
}

#[test]
fn process_created_at_current_process_returns_some() {
    let created = process_created_at(std::process::id());
    assert!(created.is_some());
    assert!(created.unwrap() > 0.0);
}

#[test]
fn process_created_at_nonexistent_returns_none() {
    assert!(process_created_at(999999).is_none());
}

#[test]
fn same_process_identity_current_process_matches() {
    let pid = std::process::id();
    let created = process_created_at(pid).unwrap();
    assert!(same_process_identity(pid, created, 2.0));
}

#[test]
fn same_process_identity_wrong_time_no_match() {
    assert!(!same_process_identity(std::process::id(), 0.0, 1.0));
}

// ── native_launch_detached tests ──

#[test]
fn native_launch_detached_rejects_empty_command_without_daemon() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let err = native_launch_detached(py, "   ".to_string(), None, None, None)
            .expect_err("empty commands should be rejected before daemon IPC");
        assert!(err.is_instance_of::<pyo3::exceptions::PyValueError>(py));
    });
}

struct ProcessTreeGuard {
    child: Option<Child>,
}

impl ProcessTreeGuard {
    fn id(&self) -> u32 {
        self.child.as_ref().expect("process tree is live").id()
    }

    fn terminate_and_reap(&mut self) -> bool {
        let pid = self.id();
        let terminator = std::thread::spawn(move || terminate_process_tree_impl(pid, 5.0));
        self.child
            .as_mut()
            .expect("process tree is live")
            .wait()
            .expect("reap process-tree root");
        self.child.take();
        terminator.join().expect("tree terminator panicked")
    }
}

impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            kill_process_tree_impl(child.id(), 1.0);
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_process_tree() -> ProcessTreeGuard {
    let mut command = if std::env::consts::OS == "windows" {
        let mut command = Command::new("cmd.exe");
        command.args([
            "/C",
            "start /B ping -n 30 127.0.0.1 >NUL & waitfor /T 30 coverage",
        ]);
        command
    } else {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30 & wait"]);
        command
    };
    ProcessTreeGuard {
        child: Some(command.spawn().expect("spawn process tree")),
    }
}

fn wait_for_descendant(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut system = System::new();
        system.refresh_processes();
        if !descendant_pids(&system, system_pid(pid)).is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "child process was not discovered"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn terminate_process_tree_kills_a_live_root_and_descendant() {
    let mut root = spawn_process_tree();
    wait_for_descendant(root.id());
    assert!(root.terminate_and_reap());
}

#[test]
fn process_tree_info_lists_a_live_descendant() {
    let mut root = spawn_process_tree();
    wait_for_descendant(root.id());
    let rendered = native_get_process_tree_info(root.id());
    assert!(rendered.contains("Child processes:"));
    assert!(root.terminate_and_reap());
}
