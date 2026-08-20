use super::*;
use std::collections::HashMap;

fn process(registered: bool, with_values: bool) -> wire::ProcessInfo {
    wire::ProcessInfo {
        key: Some(wire::ProcessKey {
            pid: 42,
            start_time: Some(1_700_000_000_000),
            boot_id: Some("boot".into()),
        }),
        name: "worker".into(),
        app_class: "coverage".into(),
        cwd: "/srv/app".into(),
        exe_path: "/usr/bin/worker".into(),
        registered,
        env: if with_values {
            HashMap::from([("MODE".into(), "test".into())])
        } else {
            HashMap::default()
        },
        env_names: vec!["MODE".into(), "TOKEN".into()],
        ..Default::default()
    }
}

#[test]
fn process_rendering_covers_empty_json_names_and_values() {
    assert_eq!(processes(&[], false), "no processes match\n");
    assert_eq!(processes(&[], true), "[]\n");

    let human = processes(&[process(true, false), process(false, true)], false);
    assert!(human.contains("PID"));
    assert!(human.contains("MODE,TOKEN"));
    assert!(human.contains("MODE=test"));
    assert!(human.contains("yes"));
    assert!(human.contains("no"));

    let json: serde_json::Value =
        serde_json::from_str(&processes(&[process(true, true)], true)).expect("process JSON");
    assert_eq!(json[0]["pid"], 42);
    assert_eq!(json[0]["env"]["MODE"], "test");
}

fn crash(id: i64) -> wire::CrashRecord {
    wire::CrashRecord {
        id,
        key: Some(wire::ProcessKey {
            pid: 77,
            start_time: Some(1_700_000_000_000),
            boot_id: Some("boot".into()),
        }),
        app_class: "coverage".into(),
        instance_name: "one".into(),
        signature: "panic::boom".into(),
        fault_kind: "signal".into(),
        crash_unix_ms: 1_700_000_000_000,
        artifact_bytes: 1_536,
        ..Default::default()
    }
}

#[test]
fn crash_rendering_covers_empty_rows_json_and_rollups() {
    assert_eq!(crashes(&[], false), "no crashes match\n");
    assert_eq!(crashes(&[], true), "[]\n");
    let human = crashes(&[crash(9)], false);
    assert!(human.contains("panic::boom"));
    assert!(human.contains("1.5 KiB"));
    let json: serde_json::Value =
        serde_json::from_str(&crashes(&[crash(9)], true)).expect("crash JSON");
    assert_eq!(json[0]["id"], 9);

    let empty = wire::CrashStatsReply::default();
    assert!(crash_stats(&empty, false).contains("0 crash(es)"));

    let stats = wire::CrashStatsReply {
        total: 3,
        distinct_classes: 2,
        first_unix_ms: 1_700_000_000_000,
        last_unix_ms: 1_700_000_010_000,
        signatures: vec![wire::CrashSignatureStat {
            signature: "panic::boom".into(),
            count: 3,
            first_unix_ms: 1_700_000_000_000,
            last_unix_ms: 1_700_000_010_000,
            app_classes: vec!["coverage".into(), "worker".into()],
        }],
        ..Default::default()
    };
    let human = crash_stats(&stats, false);
    assert!(human.contains("3 crash(es) across 2 class(es)"));
    assert!(human.contains("coverage,worker"));
    let json: serde_json::Value =
        serde_json::from_str(&crash_stats(&stats, true)).expect("stats JSON");
    assert_eq!(json["signatures"][0]["count"], 3);
}

#[test]
fn capture_and_doctor_cover_both_output_shapes() {
    let statuses = vec![
        CaptureStatus {
            pid: 1,
            job_id: String::new(),
            detail: "inline".into(),
        },
        CaptureStatus {
            pid: 2,
            job_id: "job-2".into(),
            detail: "queued".into(),
        },
    ];
    assert!(capture(&statuses, false).contains("(inline)"));
    let json: serde_json::Value =
        serde_json::from_str(&capture(&statuses, true)).expect("capture JSON");
    assert_eq!(json[1]["job_id"], "job-2");

    let checks = vec![
        ("socket".into(), true, "ready".into()),
        ("symbolizer".into(), false, "missing".into()),
    ];
    let human = doctor(&checks, false);
    assert!(human.contains("ok"));
    assert!(human.contains("FAIL"));
    let json: serde_json::Value =
        serde_json::from_str(&doctor(&checks, true)).expect("doctor JSON");
    assert_eq!(json[1]["ok"], false);
}

#[test]
fn primitive_formatters_cover_boundaries() {
    assert_eq!(millis(0), "-");
    assert_eq!(millis(1_700_000_000_000), "2023-11-14 22:13:20Z");
    assert_eq!(bytes(0), "0 B");
    assert_eq!(bytes(1_023), "1023 B");
    assert_eq!(bytes(1_024), "1.0 KiB");
    assert_eq!(bytes(1_048_576), "1.0 MiB");
    assert_eq!(bytes(1_073_741_824), "1.0 GiB");
    assert_eq!(bytes(u64::MAX), "17179869184.0 GiB");
}
