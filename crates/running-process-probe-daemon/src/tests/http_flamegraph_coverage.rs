use super::*;
use crate::probe_ops::ProbeOps;
use crate::profile::symbolize::Frame;
use crate::profile::{ProfileMetrics, ResolvedSample, SessionResult};
use crate::registry::Registry;
use running_process::broker::server::PeerCredentialPolicy;
use std::sync::Arc;

fn state() -> HttpState {
    let owner = "flame-coverage".to_string();
    let ops = Arc::new(ProbeOps::new(
        Arc::new(Registry::new(owner.clone())),
        PeerCredentialPolicy::OwnerOnly { uid_or_sid: owner },
    ));
    HttpState::new(ops, "token".into())
}

fn result() -> SessionResult {
    SessionResult {
        samples: vec![ResolvedSample {
            os_tid: 1,
            frames: vec![
                Frame {
                    function: "leaf".into(),
                    module: "fixture".into(),
                    relative_address: 1,
                },
                Frame {
                    function: "root".into(),
                    module: "fixture".into(),
                    relative_address: 2,
                },
            ],
            truncated: false,
        }],
        metrics: ProfileMetrics {
            samples_captured: 1,
            samples_dropped: 1,
            threads_seen: 1,
            threads_at_start: 2,
            pause_nanos: 1,
            duration_nanos: 100,
            hz: 99,
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn body(response: Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    String::from_utf8(bytes.to_vec()).expect("UTF-8 body")
}

async fn body_bytes(response: Response) -> axum::body::Bytes {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body")
}

#[tokio::test]
async fn page_covers_missing_and_retained_profiles() {
    let state = state();
    let missing = page(State(state.clone()), Path(99)).await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    let id = state.profiles().insert(result());
    let response = page(State(state), Path(id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_SECURITY_POLICY],
        FLAME_CSP
    );
    let html = body(response).await;
    assert!(html.contains(&format!("profile {id}")));
    assert!(html.contains("1 samples, 1 dropped, 50% thread coverage, 1.00% overhead"));
    assert!(html.contains("const PROFILE"));
}

#[tokio::test]
async fn latest_tree_reports_empty_then_returns_the_newest_profile() {
    let state = state();
    let error = tree(State(state.clone()), Query(FlameQuery::default()))
        .await
        .unwrap_err();
    assert_eq!(error.0, StatusCode::NOT_FOUND);

    state.profiles().insert(result());
    state.profiles().insert(result());
    let axum::Json(tree) = tree(State(state), Query(FlameQuery::default()))
        .await
        .expect("latest tree");
    assert_eq!(tree.value, 1);
    assert_eq!(tree.children[0].name, "root");
}

#[tokio::test]
async fn downloads_cover_every_format_and_error() {
    let state = state();
    let missing = download(State(state.clone()), Path((99, "json".into()))).await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    let id = state.profiles().insert(result());
    for (format, content_type, suffix) in [
        ("pprof", "application/octet-stream", ".pb.gz\""),
        ("pb.gz", "application/octet-stream", ".pb.gz\""),
        ("json", "application/json", ".json\""),
        ("collapsed", "text/plain; charset=utf-8", ".collapsed\""),
    ] {
        let response = download(State(state.clone()), Path((id, format.into()))).await;
        assert_eq!(response.status(), StatusCode::OK, "{format}");
        assert_eq!(response.headers()[header::CONTENT_TYPE], content_type);
        assert!(response.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .ends_with(suffix));
        assert!(!body_bytes(response).await.is_empty());
    }

    let unknown = download(State(state), Path((id, "svg".into()))).await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    assert!(body(unknown).await.contains("unknown format"));
}

#[test]
fn html_renderer_is_self_contained_and_carries_tree_data() {
    let tree = FlameNode {
        name: "root".into(),
        value: 3,
        children: vec![FlameNode {
            name: "leaf".into(),
            value: 3,
            children: Vec::new(),
        }],
    };
    let html = render_html(&tree, "Coverage", "three samples");
    assert!(html.starts_with("<!DOCTYPE html>"));
    assert!(html.contains("<title>Coverage"));
    assert!(html.contains("three samples"));
    assert!(html.contains("\"leaf\""));
    assert!(html.contains(flame_css()));
    assert!(html.contains(FLAME_JS));
}
