use super::*;

#[test]
fn report_rendering_and_panic_classification_cover_stable_operator_contracts() {
    let report = DoctorReport {
        checks: vec![
            DoctorCheck::pass("pass", "healthy"),
            DoctorCheck::warn("warn-long", "suspicious"),
            DoctorCheck::fail("fail", "broken"),
        ],
    };
    assert!(report.has_failures());
    assert_eq!(report.exit_code(), 1);
    assert_eq!(DoctorStatus::Pass.as_str(), "PASS");
    assert_eq!(DoctorStatus::Warn.as_str(), "WARN");
    assert_eq!(DoctorStatus::Fail.as_str(), "FAIL");
    let json: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
    assert_eq!(json["exit_code"], 1);
    assert_eq!(json["checks"][2]["status"], "FAIL");
    let text = report.render_text();
    assert!(text.contains("1 pass, 1 warn, 1 fail"));

    assert_eq!(panic_message(&"borrowed"), "borrowed");
    assert_eq!(panic_message(&"owned".to_string()), "owned");
    assert_eq!(panic_message(&7_u32), "non-string panic payload");
    let converted = isolated("area", || std::panic::panic_any(7_u32));
    assert_eq!(converted[0].status, DoctorStatus::Fail);
    assert!(converted[0].detail.contains("non-string"));
}

#[test]
fn endpoint_and_service_definition_checks_cover_absent_invalid_and_empty_states() {
    assert_eq!(
        pipe_path_string(Some("pipe".into()), Some(PathBuf::from("socket"))).as_deref(),
        Some("pipe")
    );
    assert_eq!(
        pipe_path_string(None, Some(PathBuf::from("socket"))).as_deref(),
        Some("socket")
    );
    assert_eq!(pipe_path_string(None, None), None);
    let unavailable = format!("running-process-doctor-missing-{}", std::process::id());
    let endpoint = broker_endpoint_check(Some(&unavailable));
    assert_eq!(endpoint.name, "broker:endpoint");
    assert_eq!(endpoint.status, DoctorStatus::Warn);

    let temp = tempfile::tempdir().unwrap();
    let missing = service_definition_checks(&temp.path().join("missing"));
    assert_eq!(missing[0].status, DoctorStatus::Warn);

    let regular_file = temp.path().join("file");
    std::fs::write(&regular_file, b"file").unwrap();
    assert_eq!(
        service_definition_checks(&regular_file)[0].status,
        DoctorStatus::Fail
    );

    let definitions = temp.path().join("definitions");
    secure_dir::ensure_private_dir(&definitions).unwrap();
    let empty = service_definition_checks(&definitions);
    assert_eq!(empty[0].status, DoctorStatus::Pass);
    assert!(empty[0].detail.contains("0 .servicedef files"));

    std::fs::write(definitions.join("broken.servicedef"), b"not a definition").unwrap();
    let invalid = service_definition_checks(&definitions);
    assert_eq!(invalid.len(), 2);
    assert_eq!(invalid[1].status, DoctorStatus::Fail);
}

#[test]
fn version_check_is_stable_and_side_effect_free() {
    let check = version_check();
    assert_eq!(check.name, "build:version");
    assert_eq!(check.status, DoctorStatus::Pass);
    assert!(check.detail.contains(env!("CARGO_PKG_VERSION")));
}
