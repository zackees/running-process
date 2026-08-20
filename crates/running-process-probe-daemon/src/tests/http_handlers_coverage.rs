use super::*;
use running_process::broker::server::PeerCredentialPolicy;
use std::sync::Arc;

use crate::crash_store::CrashStore;
use crate::probe_ops::{IdentityVerdict, ProbeErrorCode, ProbeOps};
use crate::registry::{AllowPolicy, Disclosure, ProcessKey, RegisterRequest, Registry, Runtime};

const OWNER: &str = "http-coverage-owner";

fn state() -> HttpState {
    let ops = ProbeOps::new(
        Arc::new(Registry::new(OWNER.into())),
        PeerCredentialPolicy::OwnerOnly {
            uid_or_sid: OWNER.into(),
        },
    );
    HttpState::new(Arc::new(ops), "coverage-token".into())
}

fn registered_state() -> (HttpState, ProcessKey) {
    let ops = Arc::new(ProbeOps::new(
        Arc::new(Registry::new(OWNER.into())),
        PeerCredentialPolicy::OwnerOnly {
            uid_or_sid: OWNER.into(),
        },
    ));
    let key = ProcessKey {
        pid: std::process::id(),
        started_at_unix_ms: 123,
        boot_id: "coverage-boot".into(),
    };
    let peer = running_process::broker::server::PeerIdentity {
        pid: key.pid,
        uid_or_sid: OWNER.into(),
    };
    let reply = ops.dispatch(
        ProbeRequest::Register(Box::new(RegisterRequest {
            key: key.clone(),
            exe_path: std::env::current_exe().unwrap(),
            exe_sha256: [7; 32],
            app_class: "coverage-class".into(),
            app_name: "coverage-app".into(),
            app_version: "1.0".into(),
            instance_name: "instance".into(),
            allow_policy: AllowPolicy {
                allow_all_ops: true,
                ..Default::default()
            },
            disclosure: Disclosure::default(),
            disclosed_cwd: Some("/coverage/work".into()),
            disclosed_env: Default::default(),
            nonce: [9; 32],
            supported_ops: vec!["stack_capture".into()],
            runtime: Runtime::Native,
            symbol_source: 2,
            symbol_manifest_path: None,
            symbol_paths: Vec::new(),
        })),
        &peer,
        77,
        IdentityVerdict {
            verified: true,
            connection_alive: true,
        },
    );
    assert!(matches!(reply, ProbeReply::Armed { .. }));
    (HttpState::new(ops, "coverage-token".into()), key)
}

fn crash_state() -> (tempfile::TempDir, HttpState) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        CrashStore::open(
            &dir.path().join("crashes.db"),
            &dir.path().join("artifacts"),
        )
        .unwrap(),
    );
    store
        .connection_for_test()
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO crashes (app_class, app_name, app_version, instance_name, pid,
                                  creation_time_ms, cwd, signature, crashed_at_ms, exit_signal,
                                  report_json, artifact_path, artifact_bytes)
             VALUES ('coverage-class', 'coverage-app', '1.0', 'instance', 7, 1,
                     '/work', 'SIGSEGV@coverage', 1000, 'SIGSEGV', '{}', 'artifact.json', 12)",
            [],
        )
        .unwrap();
    let ops = ProbeOps::new(
        Arc::new(Registry::new(OWNER.into())),
        PeerCredentialPolicy::OwnerOnly {
            uid_or_sid: OWNER.into(),
        },
    )
    .with_crash_store(store);
    (dir, HttpState::new(Arc::new(ops), "coverage-token".into()))
}

#[test]
fn api_error_crash_filter_and_refusal_shapes_preserve_public_details() {
    let (status, Json(error)) = ApiError::new("invalid", "detail");
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error.error, "invalid");
    assert_eq!(error.detail, "detail");

    let params = CrashParams {
        class: Some("class".into()),
        class_like: Some("class%".into()),
        signature: Some("signature".into()),
        since: Some(10),
        until: Some(20),
        limit: Some(5),
    };
    let filter = params.filter();
    assert_eq!(filter.app_class.as_deref(), Some("class"));
    assert_eq!(filter.app_class_like.as_deref(), Some("class%"));
    assert_eq!(filter.signature.as_deref(), Some("signature"));
    assert_eq!(filter.since_unix_ms, Some(10));
    assert_eq!(filter.until_unix_ms, Some(20));

    for reply in [
        ProbeReply::Refused {
            code: ProbeErrorCode::PolicyDenied,
            reason: "policy".into(),
        },
        ProbeReply::CrashRefused {
            code: ProbeErrorCode::NotRegistered,
            reason: "store".into(),
            stats: false,
        },
    ] {
        let (status, Json(error)) = refusal(reply);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!error.error.is_empty());
        assert!(!error.detail.is_empty());
    }

    let (status, Json(error)) = refusal(ProbeReply::Ack);
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(error.error, "unexpected_reply");
    assert!(error.detail.contains("Ack"));
}

#[tokio::test]
async fn ps_covers_defaults_all_selectors_and_invalid_regex() {
    let Json(rows) = ps(State(state()), Query(PsParams::default()))
        .await
        .unwrap();
    assert!(rows.is_empty());

    let Json(rows) = ps(
        State(state()),
        Query(PsParams {
            name: Some("missing*".into()),
            name_regex: None,
            cwd: Some("/missing/*".into()),
            app_class: Some("missing".into()),
            include_unregistered: false,
            include_env: true,
            limit: Some(1),
        }),
    )
    .await
    .unwrap();
    assert!(rows.is_empty());

    let (status, Json(error)) = ps(
        State(state()),
        Query(PsParams {
            name_regex: Some("(".into()),
            ..Default::default()
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error.error, "invalid_query");
}

#[tokio::test]
async fn ps_and_snapshot_project_registered_process_and_capture_receipt() {
    let (state, key) = registered_state();
    let Json(rows) = ps(
        State(state.clone()),
        Query(PsParams {
            name: Some("*".into()),
            cwd: Some("/coverage/*".into()),
            app_class: Some("coverage-class".into()),
            include_env: true,
            limit: Some(10),
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].pid, u64::from(key.pid));
    assert_eq!(rows[0].app_class, "coverage-class");
    assert_eq!(rows[0].app_name, "coverage-app");
    assert_eq!(rows[0].cwd, "/coverage/work");
    assert!(rows[0].registered);

    let Json(receipt) = snapshot(
        State(state),
        Query(SnapshotParams {
            pid: key.pid,
            start_time: key.started_at_unix_ms,
            boot_id: key.boot_id,
            max_depth: 32,
        }),
    )
    .await
    .unwrap();
    assert!(!receipt.job_id.is_empty());
    assert_eq!(receipt.state, 0);
}

#[tokio::test]
async fn crash_routes_cover_zero_limit_and_missing_store_refusals() {
    let (status, Json(error)) = crashes(
        State(state()),
        Query(CrashParams {
            limit: Some(0),
            ..Default::default()
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error.error, "invalid_query");

    let (status, Json(error)) = crashes(
        State(state()),
        Query(CrashParams {
            limit: Some(u32::MAX),
            ..Default::default()
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error.detail.contains("no crash history store"));

    let (status, Json(error)) = crash_stats(State(state()), Query(CrashParams::default()))
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error.detail.contains("no crash history store"));
}

#[tokio::test]
async fn crash_routes_project_rows_and_signature_statistics() {
    let (_dir, state) = crash_state();
    let params = CrashParams {
        class: Some("coverage-class".into()),
        class_like: None,
        signature: Some("SIGSEGV@coverage".into()),
        since: Some(500),
        until: Some(1500),
        limit: Some(u32::MAX),
    };
    let Json(rows) = crashes(State(state.clone()), Query(params)).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].app_class, "coverage-class");
    assert_eq!(rows[0].app_name, "coverage-app");
    assert_eq!(rows[0].instance_name, "instance");
    assert_eq!(rows[0].pid, 7);
    assert_eq!(rows[0].signature, "SIGSEGV@coverage");
    assert_eq!(rows[0].fault_kind, "SIGSEGV");
    assert_eq!(rows[0].crashed_at_ms, 1000);
    assert_eq!(rows[0].artifact_bytes, 12);

    let Json(stats) = crash_stats(
        State(state),
        Query(CrashParams {
            class_like: Some("coverage%".into()),
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    assert_eq!(stats.total, 1);
    assert_eq!(stats.first_unix_ms, 1000);
    assert_eq!(stats.last_unix_ms, 1000);
    assert_eq!(stats.distinct_classes, 1);
    assert_eq!(stats.signatures.len(), 1);
    assert_eq!(stats.signatures[0].signature, "SIGSEGV@coverage");
    assert_eq!(stats.signatures[0].count, 1);
    assert_eq!(stats.signatures[0].app_classes, ["coverage-class"]);
}

#[tokio::test]
async fn snapshot_missing_process_and_empty_profiles_are_stable() {
    let (status, Json(error)) = snapshot(
        State(state()),
        Query(SnapshotParams {
            pid: 42,
            start_time: 7,
            boot_id: "boot".into(),
            max_depth: 64,
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error.error, "NotRegistered");
    assert!(!error.detail.is_empty());

    let Json(ids) = profiles(State(state())).await.unwrap();
    assert!(ids.is_empty());
}
