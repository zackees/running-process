use super::*;
use crate::broker::manifest;
use crate::broker::protocol::{CacheManifest, DaemonProcess};
use std::path::PathBuf;

#[test]
fn verifier_reports_corrupt_stale_host_boot_and_dead_daemon_entries() {
    let temp = tempfile::tempdir().unwrap();
    let registry = temp.path().join("registry");
    manifest::ensure_central_registry_dir(&registry).unwrap();
    std::fs::write(registry.join("broken.pb"), b"not protobuf").unwrap();

    let current = crate::broker::host_identity::current();
    let manifest = CacheManifest {
        host: Some(crate::broker::protocol::HostIdentity {
            machine_id: format!("{}-stale", current.machine_id),
            boot_id: format!("{}-stale", current.boot_id),
            ..current
        }),
        current_daemon: Some(DaemonProcess {
            pid: 0,
            ..Default::default()
        }),
        ..Default::default()
    };
    manifest::write_to_central_in_dir(&registry, "coverage", "1.0.0", &manifest).unwrap();

    let report = run(&registry);
    assert_eq!(report.scanned, 2);
    assert!(report.findings.iter().any(|item| item.severity == "error"));
    for message in ["another machine", "prior boot", "daemon pid 0 is not alive"] {
        assert!(report
            .findings
            .iter()
            .any(|item| item.message.contains(message)));
    }
    let json: serde_json::Value = serde_json::from_str(&render_json(&report)).unwrap();
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["scanned"], 2);
}

#[test]
fn verifier_handles_missing_registry_and_json_escapes_findings() {
    let temp = tempfile::tempdir().unwrap();
    let report = run(&temp.path().join("missing"));
    assert_eq!(
        report,
        VerifyReport {
            scanned: 0,
            findings: vec![]
        }
    );
    assert!(!process_is_alive(0));
    assert!(process_is_alive(std::process::id()));

    let rendered = render_json(&VerifyReport {
        scanned: 1,
        findings: vec![VerifyFinding {
            path: PathBuf::from("quoted\"path"),
            severity: "error",
            message: "line\nslash\\".into(),
        }],
    });
    let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(value["findings"][0]["path"], "quoted\"path");
    assert_eq!(value["findings"][0]["message"], "line\nslash\\");
}
