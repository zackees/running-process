//! Cooperative capture producer for daemon-leased #637 work.
//!
//! This code runs inside the registered process. That is the only address
//! space where captured absolute addresses can be attributed to loaded
//! modules before ASLR makes them meaningless. The artifact handed to the
//! daemon contains module indexes and relative offsets only.

use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use running_process_probe::probe_diag::v1::{CaptureReply, CaptureStackRequest};
use running_process_probe::snapshot::{
    attribute::{attribute, AttributedCapture},
    capture_and_resolve,
    modules::enumerate_modules,
    SnapshotConfig,
};
use serde::Serialize;

const DEFAULT_MAX_DEPTH: usize = 256;

#[derive(Serialize)]
struct RawCapture<'a> {
    format: &'static str,
    modules: Vec<RawModule<'a>>,
    threads: Vec<RawThread>,
}

#[derive(Serialize)]
struct RawModule<'a> {
    name: &'a str,
    base_avma: u64,
    path_hint: Option<&'a str>,
}

#[derive(Serialize)]
struct RawThread {
    os_tid: u64,
    frames: Vec<RawFrame>,
    py_frames: Vec<RawPyFrame>,
}

#[derive(Serialize)]
struct RawFrame {
    module_index: u32,
    relative_address: u64,
}

#[derive(Serialize)]
struct RawPyFrame {
    file: String,
    line: u32,
    func: String,
}

/// Capture this process and return the artifact reference used by the wire.
pub(super) fn capture(request: &CaptureStackRequest) -> CaptureReply {
    let started_unix_ms = unix_millis();
    match capture_inner(request) {
        Ok((path, threads_captured, threads_dropped, pause_ns)) => CaptureReply {
            started_unix_ms,
            artifact_path: path.to_string_lossy().into_owned(),
            threads_captured,
            threads_dropped,
            pause_ns,
            ..Default::default()
        },
        Err(error) => CaptureReply {
            started_unix_ms,
            // PROBE_ERROR_INTERNAL. The daemon records this against the
            // leased job; the application itself stays alive and registered.
            error: 5,
            detail: error.to_string(),
            ..Default::default()
        },
    }
}

fn capture_inner(request: &CaptureStackRequest) -> io::Result<(PathBuf, u32, u32, u64)> {
    let snapshot = capture_and_resolve(&SnapshotConfig::default())
        .map_err(|error| io::Error::other(format!("cooperative capture failed: {error}")))?;
    let stats = snapshot.stats;
    let modules = enumerate_modules()
        .map_err(|error| io::Error::other(format!("module inventory failed: {error}")))?;
    let attributed = attribute(&snapshot, &modules);
    let payload = worker_payload(&attributed, request.max_depth, request.thread_filter);
    let path = create_artifact(&payload)?;
    Ok((
        path,
        stats.threads_captured,
        stats.threads_dropped,
        stats.pause_nanos,
    ))
}

fn worker_payload(
    capture: &AttributedCapture,
    max_depth: u32,
    thread_filter: u32,
) -> RawCapture<'_> {
    let depth = if max_depth == 0 {
        DEFAULT_MAX_DEPTH
    } else {
        max_depth as usize
    };
    let modules = capture
        .modules
        .iter()
        .map(|module| RawModule {
            name: &module.name,
            base_avma: module.base,
            path_hint: module.path.as_deref(),
        })
        .collect();
    let threads = capture
        .threads
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            thread_filter == 0
                || (*index < u32::BITS as usize && thread_filter & (1 << *index) != 0)
        })
        .map(|(_, thread)| RawThread {
            os_tid: thread.os_tid,
            frames: thread
                .frames
                .iter()
                .take(depth)
                .map(|frame| RawFrame {
                    // An out-of-range index is the worker wire's explicit
                    // ModuleUnknown representation. Never guess an owner.
                    module_index: frame.module_index.unwrap_or(u32::MAX),
                    relative_address: frame.relative_address,
                })
                .collect(),
            // Native registrations have no interpreter. Python's separate
            // mixed-mode producer populates this same field before invoking
            // the worker; an empty list is explicit rather than omitted.
            py_frames: Vec::new(),
        })
        .collect();
    RawCapture {
        format: "cooperative_frames",
        modules,
        threads,
    }
}

fn create_artifact(payload: &RawCapture<'_>) -> io::Result<PathBuf> {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
    let token = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let path = std::env::temp_dir().join(format!(
        "rp-probe-capture-{}-{token}.json",
        std::process::id()
    ));

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    serde_json::to_writer(&mut file, payload)
        .map_err(|error| io::Error::other(format!("encode capture: {error}")))?;
    file.flush()?;
    Ok(path)
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use running_process_probe::snapshot::attribute::{
        AttributedFrame, AttributedModule, AttributedThread,
    };

    #[test]
    fn worker_payload_is_bounded_and_keeps_unknown_frames_explicit() {
        let capture = AttributedCapture {
            modules: vec![AttributedModule {
                name: "app.exe".into(),
                path: Some("app.exe".into()),
                base: 0x1000,
            }],
            threads: vec![AttributedThread {
                os_tid: 7,
                frames: vec![
                    AttributedFrame {
                        module_index: Some(0),
                        relative_address: 0x10,
                    },
                    AttributedFrame {
                        module_index: None,
                        relative_address: 0xDEAD,
                    },
                ],
            }],
        };
        let payload = worker_payload(&capture, 2, 0);
        assert_eq!(payload.threads.len(), 1);
        assert_eq!(payload.threads[0].frames[0].module_index, 0);
        assert_eq!(payload.threads[0].frames[1].module_index, u32::MAX);
        assert_eq!(payload.threads[0].frames[1].relative_address, 0xDEAD);
    }
}
