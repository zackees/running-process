//! Turning raw frames into a report (#637).
//!
//! This slice establishes the contract and the attribution step: every frame
//! is matched to its module and keeps its offset, and each frame carries a
//! status saying how far resolution got. Reading function names out of symbol
//! files is the next slice; the shape here is what it plugs into.
//!
//! The reason attribution is worth landing on its own is that it is the part
//! that must never be wrong. A frame attributed to the wrong module would make
//! a later, correct symbol lookup produce a confidently wrong name.

use crate::wire::{
    CaptureFormat, FrameStatus, RawCapture, RawThread, SymFrame, SymThread, SymbolReport,
};

/// Why symbolization could not produce a report.
#[derive(Debug, thiserror::Error)]
pub enum SymbolizeError {
    /// The capture asked for a path this build does not implement.
    #[error("capture format {0:?} is not supported yet")]
    UnsupportedFormat(CaptureFormat),
}

/// Name used when a frame cannot be attributed to any module.
///
/// A visible placeholder rather than an empty string: a report reader should
/// be able to tell "unattributable" from "the module had no name".
pub const UNKNOWN_MODULE: &str = "<unknown>";

/// Symbolize a capture.
///
/// Threads, their order, their ids, and their interpreter frames all pass
/// through unchanged — only the native frames are touched.
pub fn symbolize(capture: &RawCapture) -> Result<SymbolReport, SymbolizeError> {
    match capture.format {
        CaptureFormat::CooperativeFrames => {}
        // Parsing a minidump requires the crash path from S7. Refusing is the
        // honest answer; returning an empty report would read as "this crash
        // had no threads".
        other => return Err(SymbolizeError::UnsupportedFormat(other)),
    }

    let threads = capture
        .threads
        .iter()
        .map(|thread| symbolize_thread(capture, thread))
        .collect();

    Ok(SymbolReport { threads })
}

fn symbolize_thread(capture: &RawCapture, thread: &RawThread) -> SymThread {
    let frames = thread
        .frames
        .iter()
        .map(|frame| {
            match capture.modules.get(frame.module_index as usize) {
                Some(module) => SymFrame {
                    module: module.name.clone(),
                    relative_address: frame.relative_address,
                    function: None,
                    file: None,
                    line: None,
                    inline_frames: Vec::new(),
                    // The module is known but no symbol source has been
                    // consulted yet, which is exactly `RawOnly`.
                    status: FrameStatus::RawOnly,
                },
                // An out-of-range index is a malformed capture, but one bad
                // frame must not discard the rest of the thread: the
                // surrounding frames are still evidence.
                None => SymFrame {
                    module: UNKNOWN_MODULE.to_string(),
                    relative_address: frame.relative_address,
                    function: None,
                    file: None,
                    line: None,
                    inline_frames: Vec::new(),
                    status: FrameStatus::ModuleUnknown,
                },
            }
        })
        .collect();

    SymThread {
        os_tid: thread.os_tid,
        name: thread.name.clone(),
        frames,
        py_frames: thread.py_frames.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{ModuleRef, PyFrame, RawFrame};

    fn capture_with(modules: Vec<ModuleRef>, frames: Vec<RawFrame>) -> RawCapture {
        RawCapture {
            format: CaptureFormat::CooperativeFrames,
            modules,
            threads: vec![RawThread {
                os_tid: 11,
                name: Some("t".into()),
                frames,
                py_frames: Vec::new(),
            }],
        }
    }

    fn module(name: &str) -> ModuleRef {
        ModuleRef {
            name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn frames_are_attributed_to_their_module() {
        let capture = capture_with(
            vec![module("a.dll"), module("b.dll")],
            vec![
                RawFrame {
                    module_index: 1,
                    relative_address: 0x20,
                },
                RawFrame {
                    module_index: 0,
                    relative_address: 0x10,
                },
            ],
        );
        let report = symbolize(&capture).unwrap();
        let frames = &report.threads[0].frames;

        assert_eq!(frames[0].module, "b.dll");
        assert_eq!(frames[0].relative_address, 0x20);
        assert_eq!(frames[1].module, "a.dll");
        assert_eq!(frames[1].relative_address, 0x10);
    }

    /// Offsets must survive even when nothing else could be determined — a
    /// report of bare offsets is still actionable against the right build.
    #[test]
    fn an_unknown_module_index_keeps_the_offset() {
        let capture = capture_with(
            vec![module("a.dll")],
            vec![RawFrame {
                module_index: 99,
                relative_address: 0xDEAD,
            }],
        );
        let frame = &symbolize(&capture).unwrap().threads[0].frames[0];

        assert_eq!(frame.status, FrameStatus::ModuleUnknown);
        assert_eq!(frame.relative_address, 0xDEAD);
        assert_eq!(frame.module, UNKNOWN_MODULE);
        assert!(frame.function.is_none());
    }

    /// One malformed frame must not cost the frames around it.
    #[test]
    fn a_bad_frame_does_not_discard_its_neighbours() {
        let capture = capture_with(
            vec![module("a.dll")],
            vec![
                RawFrame {
                    module_index: 0,
                    relative_address: 1,
                },
                RawFrame {
                    module_index: 7,
                    relative_address: 2,
                },
                RawFrame {
                    module_index: 0,
                    relative_address: 3,
                },
            ],
        );
        let frames = &symbolize(&capture).unwrap().threads[0].frames;

        assert_eq!(frames.len(), 3, "no frame may be dropped");
        assert_eq!(frames[0].status, FrameStatus::RawOnly);
        assert_eq!(frames[1].status, FrameStatus::ModuleUnknown);
        assert_eq!(frames[2].status, FrameStatus::RawOnly);
    }

    /// No symbol source has been consulted, so nothing may claim `Resolved`.
    #[test]
    fn nothing_is_reported_as_resolved_without_symbols() {
        let capture = capture_with(
            vec![module("a.dll")],
            vec![RawFrame {
                module_index: 0,
                relative_address: 0x40,
            }],
        );
        for frame in &symbolize(&capture).unwrap().threads[0].frames {
            assert_ne!(
                frame.status,
                FrameStatus::Resolved,
                "a name was claimed without any symbol file being read"
            );
            assert!(frame.function.is_none());
        }
    }

    #[test]
    fn python_frames_pass_through_untouched() {
        let py = PyFrame {
            file: "app.py".into(),
            line: 12,
            func: "handler".into(),
        };
        let mut capture = capture_with(vec![module("a.dll")], Vec::new());
        capture.threads[0].py_frames = vec![py.clone()];

        let thread = &symbolize(&capture).unwrap().threads[0];
        assert_eq!(thread.py_frames, vec![py]);
    }

    /// The tid is the join key for mixed-mode pairing; losing it would
    /// silently unpair the two halves the client matched up.
    #[test]
    fn thread_identity_and_order_survive() {
        let capture = RawCapture {
            format: CaptureFormat::CooperativeFrames,
            modules: vec![module("a.dll")],
            threads: vec![
                RawThread {
                    os_tid: 100,
                    name: Some("first".into()),
                    ..Default::default()
                },
                RawThread {
                    os_tid: 200,
                    name: None,
                    ..Default::default()
                },
            ],
        };
        let report = symbolize(&capture).unwrap();

        assert_eq!(report.threads.len(), 2);
        assert_eq!(report.threads[0].os_tid, 100);
        assert_eq!(report.threads[0].name.as_deref(), Some("first"));
        assert_eq!(report.threads[1].os_tid, 200);
        assert_eq!(report.threads[1].name, None);
    }

    /// An unimplemented path must refuse rather than return an empty report,
    /// which would read as "this crash had no threads".
    #[test]
    fn the_minidump_path_refuses_rather_than_returning_nothing() {
        let capture = RawCapture {
            format: CaptureFormat::Minidump,
            ..Default::default()
        };
        assert!(matches!(
            symbolize(&capture),
            Err(SymbolizeError::UnsupportedFormat(CaptureFormat::Minidump))
        ));
    }
}
