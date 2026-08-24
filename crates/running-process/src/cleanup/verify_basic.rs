use std::path::Path;

use crate::broker::{host_identity, manifest};
use crate::cleanup::json_escape;

/// One manifest verification finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyFinding {
    /// Manifest path.
    pub path: std::path::PathBuf,
    /// Finding severity.
    pub severity: &'static str,
    /// Human-readable message.
    pub message: String,
}

/// Basic v1 verification result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// Number of `.pb` entries scanned.
    pub scanned: usize,
    /// Findings generated during verification.
    pub findings: Vec<VerifyFinding>,
}

/// Run basic verification over the central registry.
pub fn run(registry_dir: &Path) -> VerifyReport {
    let current = host_identity::current();
    let entries = manifest::scan_central(registry_dir);
    let mut findings = Vec::new();
    let scanned = entries.len();

    for entry in entries {
        match entry.result {
            Ok(manifest) => {
                if let Some(host) = manifest.host.as_ref() {
                    if !host.machine_id.is_empty() && host.machine_id != current.machine_id {
                        findings.push(VerifyFinding {
                            path: entry.path.clone(),
                            severity: "stale",
                            message: "manifest belongs to another machine".to_string(),
                        });
                    }
                    if !host.boot_id.is_empty() && host.boot_id != current.boot_id {
                        findings.push(VerifyFinding {
                            path: entry.path.clone(),
                            severity: "stale",
                            message: "manifest belongs to a prior boot".to_string(),
                        });
                    }
                }
                if let Some(daemon) = manifest.current_daemon.as_ref() {
                    if !process_is_alive(daemon.pid) {
                        findings.push(VerifyFinding {
                            path: entry.path,
                            severity: "stale",
                            message: format!("daemon pid {} is not alive", daemon.pid),
                        });
                    }
                }
            }
            Err(err) => findings.push(VerifyFinding {
                path: entry.path,
                severity: "error",
                message: err.to_string(),
            }),
        }
    }

    VerifyReport { scanned, findings }
}

/// Render `running-process-cleanup verify --json`.
pub fn render_json(report: &VerifyReport) -> String {
    let findings = report
        .findings
        .iter()
        .map(|finding| {
            format!(
                "{{\"path\":\"{}\",\"severity\":\"{}\",\"message\":\"{}\"}}",
                json_escape(&finding.path.to_string_lossy()),
                finding.severity,
                json_escape(&finding.message)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"schema_version\":1,\"scanned\":{},\"findings\":[{}]}}",
        report.scanned, findings
    )
}

#[cfg(test)]
#[path = "../tests/verify_basic_coverage.rs"]
mod coverage_tests;

/// Whether a PID still names a running process.
///
/// This used to be two hand-rolled implementations, one per host. The Unix
/// one signalled zero and read `errno`; the Windows one opened a handle and
/// called success "alive" -- which it is not, because a process that has
/// exited can still be opened while any handle to it remains, and would have
/// been reported as running.
///
/// `verify_pid` already asks `platform::process`, which holds a reference
/// the kernel will not recycle and checks the exit status rather than the
/// handle. `cleanup/verify_artifacts.rs` next door already calls it.
fn process_is_alive(pid: u32) -> bool {
    crate::broker::backend_lifecycle::verify_pid::process_is_alive(pid)
}
