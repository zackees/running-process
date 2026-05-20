//! Process group with originator-env injection that delegates to the
//! two-mode [`crate::spawn`] surface.
//!
//! `ContainedProcessGroup` no longer carries OS-level containment state of
//! its own (the new `spawn` builds a Job Object per-spawn on Windows and
//! places each child in its own process group on Unix). The group's
//! responsibility is now scoped to:
//!
//! - holding an optional `originator` label,
//! - injecting [`ORIGINATOR_ENV_VAR`] into every child the group spawns,
//! - dispatching to either [`crate::spawn`] or [`crate::spawn_daemon`].
//!
//! # `RUNNING_PROCESS_ORIGINATOR` environment variable
//!
//! When an `originator` is set on a `ContainedProcessGroup`, all spawned child
//! processes inherit the environment variable `RUNNING_PROCESS_ORIGINATOR` with
//! the format `TOOL:PID`, where:
//!
//! - **TOOL** is the originator name (e.g., `"CLUD"`, `"JUPYTER"`)
//! - **PID** is the process ID of the parent that spawned the group
//!
//! Example value: `RUNNING_PROCESS_ORIGINATOR=CLUD:12345`
//!
//! ## Purpose
//!
//! This env var enables **cross-process session discovery** after crashes.
//!
//! ## Example
//!
//! ```no_run
//! use running_process_core::{ContainedProcessGroup, SpawnStdio};
//!
//! let group = ContainedProcessGroup::with_originator("CLUD").unwrap();
//! let mut cmd = std::process::Command::new("sleep");
//! cmd.arg("60");
//! let _child = group.spawn(&mut cmd, SpawnStdio::default()).unwrap();
//! ```

use std::process::Command;

use crate::spawn::{
    spawn as free_spawn, spawn_daemon as free_spawn_daemon, DaemonChild, SpawnStdio, SpawnedChild,
};

/// The environment variable name injected into child processes for
/// cross-process session discovery.
pub const ORIGINATOR_ENV_VAR: &str = "RUNNING_PROCESS_ORIGINATOR";

/// A logical group of spawned processes that share an originator label.
///
/// Each [`ContainedProcessGroup::spawn`] call builds its own OS-level
/// containment (Job Object on Windows, process-group on Unix), so the
/// group itself is just metadata.
pub struct ContainedProcessGroup {
    originator: Option<String>,
}

/// Format the originator env var value: `TOOL:PID`.
fn format_originator_value(tool: &str) -> String {
    format!("{}:{}", tool, std::process::id())
}

impl ContainedProcessGroup {
    /// Create a new process group without an originator.
    pub fn new() -> Result<Self, std::io::Error> {
        Ok(Self { originator: None })
    }

    /// Create a new process group with an originator name.
    pub fn with_originator(originator: &str) -> Result<Self, std::io::Error> {
        Ok(Self {
            originator: Some(originator.to_string()),
        })
    }

    /// Returns the originator name, if set.
    pub fn originator(&self) -> Option<&str> {
        self.originator.as_deref()
    }

    /// Returns the full originator env var value (`TOOL:PID`), if set.
    pub fn originator_value(&self) -> Option<String> {
        self.originator.as_ref().map(|o| format_originator_value(o))
    }

    fn inject_originator_env(&self, command: &mut Command) {
        if let Some(ref originator) = self.originator {
            command.env(ORIGINATOR_ENV_VAR, format_originator_value(originator));
        }
    }

    /// Spawn a contained child process. The child is contained by its own
    /// Job Object on Windows / process group on Unix and is killed when
    /// the returned [`SpawnedChild`] is dropped.
    pub fn spawn(
        &self,
        command: &mut Command,
        stdio: SpawnStdio<'_>,
    ) -> Result<SpawnedChild, std::io::Error> {
        self.inject_originator_env(command);
        free_spawn(command, stdio)
    }

    /// Spawn a detached daemon child. The child has NUL stdio, a sanitized
    /// handle list, and survives the returned [`DaemonChild`] being
    /// dropped. To terminate, call [`DaemonChild::kill`].
    ///
    /// The parent-child association (this group's originator env var)
    /// is injected into the child before the spawn so cross-process
    /// tracking can resolve the spawned daemon back to its parent.
    pub fn spawn_daemon(&self, command: &mut Command) -> Result<DaemonChild, std::io::Error> {
        self.inject_originator_env(command);
        free_spawn_daemon(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contained_process_group_creates_successfully() {
        let group = ContainedProcessGroup::new();
        assert!(group.is_ok());
    }

    #[test]
    fn with_originator_creates_successfully() {
        let group = ContainedProcessGroup::with_originator("CLUD");
        assert!(group.is_ok());
        let group = group.unwrap();
        assert_eq!(group.originator(), Some("CLUD"));
    }

    #[test]
    fn originator_value_format() {
        let group = ContainedProcessGroup::with_originator("CLUD").unwrap();
        let value = group.originator_value().unwrap();
        let expected = format!("CLUD:{}", std::process::id());
        assert_eq!(value, expected);
    }

    #[test]
    fn no_originator_returns_none() {
        let group = ContainedProcessGroup::new().unwrap();
        assert!(group.originator().is_none());
        assert!(group.originator_value().is_none());
    }

    #[test]
    fn format_originator_value_correct() {
        let value = format_originator_value("JUPYTER");
        let parts: Vec<&str> = value.splitn(2, ':').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "JUPYTER");
        assert_eq!(parts[1], std::process::id().to_string());
    }
}
