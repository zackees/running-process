use super::*;
use running_process_probe::snapshot::attribute::{
    AttributedFrame, AttributedModule, AttributedThread,
};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

struct ArtifactGuard(PathBuf);

impl ArtifactGuard {
    fn new(path: PathBuf) -> Self {
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ArtifactGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn attributed(path: Option<String>) -> AttributedCapture {
    AttributedCapture {
        modules: vec![AttributedModule {
            name: "fixture".into(),
            path,
            debug_id: Some("debug".into()),
            debug_file: Some("fixture.debug".into()),
            base: 0x1000,
        }],
        threads: vec![
            AttributedThread {
                os_tid: 10,
                frames: vec![
                    AttributedFrame {
                        module_index: Some(0),
                        relative_address: 1,
                    },
                    AttributedFrame {
                        module_index: None,
                        relative_address: 2,
                    },
                ],
            },
            AttributedThread {
                os_tid: 11,
                frames: vec![AttributedFrame {
                    module_index: Some(0),
                    relative_address: 3,
                }],
            },
        ],
    }
}

#[test]
fn worker_payload_applies_default_depth_thread_filter_and_discovery() {
    let request = CaptureStackRequest {
        thread_filter: 0b10,
        symbol_paths: vec!["symbols-a".into(), "symbols-b".into()],
        ..Default::default()
    };
    let payload = worker_payload(&attributed(None), &request);
    assert_eq!(payload.format, "cooperative_frames");
    assert_eq!(payload.threads.len(), 1);
    assert_eq!(payload.threads[0].os_tid, 11);
    assert!(payload.threads[0].py_frames.is_empty());
    assert_eq!(payload.discovery.registered_manifest, None);
    assert_eq!(payload.discovery.registered_symbol_paths.len(), 2);
    assert_eq!(payload.modules[0].code_id, None);
    assert_eq!(payload.modules[0].path_hint, None);

    let limited = worker_payload(
        &attributed(None),
        &CaptureStackRequest {
            max_depth: 1,
            symbol_manifest_path: "manifest.json".into(),
            ..Default::default()
        },
    );
    assert_eq!(limited.threads.len(), 2);
    assert_eq!(limited.threads[0].frames.len(), 1);
    assert_eq!(
        limited.discovery.registered_manifest.as_deref(),
        Some("manifest.json")
    );
}

#[test]
fn file_identity_distinguishes_hashable_missing_and_oversized_images() {
    let temp = tempfile::tempdir().unwrap();
    let small = temp.path().join("small.bin");
    std::fs::write(&small, b"identity").unwrap();
    let identity = captured_file_identity(&small.to_string_lossy()).unwrap();
    assert!(identity.starts_with("sha256:"));
    assert_eq!(identity.len(), 71);

    let missing = temp.path().join("missing.bin");
    assert_eq!(
        captured_file_identity(&missing.to_string_lossy()).as_deref(),
        Some("sha256:unavailable")
    );

    let large = temp.path().join("large.bin");
    let file = std::fs::File::create(&large).unwrap();
    file.set_len(MAX_CAPTURE_IDENTITY_BYTES + 1).unwrap();
    assert_eq!(
        captured_file_identity(&large.to_string_lossy()).as_deref(),
        Some("sha256:unavailable")
    );
}

#[test]
fn artifact_creation_writes_private_json_and_live_capture_returns_evidence() {
    let payload = worker_payload(&attributed(None), &CaptureStackRequest::default());
    let artifact = ArtifactGuard::new(create_artifact(&payload).unwrap());
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(artifact.path()).unwrap()).unwrap();
    assert_eq!(value["format"], "cooperative_frames");

    // A capture samples *sibling* threads -- every backend excludes the
    // calling thread -- so asserting a nonzero count means first guaranteeing
    // a sibling exists. Without one the count is whatever else the test binary
    // happens to be running, which is why this assertion failed under load on
    // a PR that touched no probe code (#1066). The probe crate's own capture
    // tests spawn their subjects for the same reason; this follows them.
    let release = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));
    let sibling = {
        let release = Arc::clone(&release);
        let ready = Arc::clone(&ready);
        std::thread::spawn(move || {
            ready.store(true, Ordering::Release);
            while !release.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
        })
    };
    while !ready.load(Ordering::Acquire) {
        std::thread::yield_now();
    }

    let reply = capture(&CaptureStackRequest {
        max_depth: 8,
        ..Default::default()
    });

    release.store(true, Ordering::Release);
    sibling.join().unwrap();

    let live_artifact = ArtifactGuard::new(PathBuf::from(&reply.artifact_path));
    assert_eq!(reply.error, 0, "{}", reply.detail);
    assert!(reply.started_unix_ms > 0);
    assert!(
        reply.threads_captured > 0,
        "the spawned sibling must appear: captured={} dropped={}",
        reply.threads_captured,
        reply.threads_dropped
    );
    assert!(live_artifact.path().is_file());
}
