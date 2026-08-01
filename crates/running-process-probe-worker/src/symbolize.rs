//! Turning raw frames into a report (#637).
//!
//! Two steps. Attribution matches every frame to its module and keeps its
//! offset; resolution asks that module's symbol file for a function name.
//! Each frame carries a status saying how far it got, and the offset survives
//! regardless — a report of bare offsets is still actionable against the right
//! build, which is what makes degradation acceptable.
//!
//! Attribution is the step that must never be wrong: a frame assigned to the
//! wrong module would make an otherwise correct symbol lookup produce a
//! confidently wrong name.
//!
//! # Platform coverage
//!
//! Resolution reads PE/PDB, which is the only capture backend that exists
//! today (#635). Elsewhere every frame stays `RawOnly` — the honest result,
//! rather than an empty report or a guess. ELF/DWARF joins when the Unix
//! capture backends land.

use crate::wire::{
    CaptureFormat, FrameStatus, ModuleReport, ModuleSymbolStatus, RawCapture, RawThread, SymFrame,
    SymThread, SymbolReport,
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

    let cache = build_symbol_cache(capture);
    let threads = capture
        .threads
        .iter()
        .map(|thread| symbolize_thread(capture, &cache, thread))
        .collect();
    let modules = module_reports(capture, &cache);

    Ok(SymbolReport { threads, modules })
}

/// Symbol tables, built once per module and reused across every frame.
///
/// Opening and sorting a PDB per frame would reparse the same file dozens of
/// times for one thread. Entries are `None` when the module has no usable
/// symbols, so a miss is remembered rather than retried.
#[cfg(target_os = "windows")]
type SymbolCache = Vec<crate::pdb_symbols::ModuleSymbols>;
#[cfg(not(target_os = "windows"))]
type SymbolCache = Vec<crate::object_symbols::ModuleSymbols>;

#[cfg(target_os = "windows")]
fn build_symbol_cache(capture: &RawCapture) -> SymbolCache {
    capture
        .modules
        .iter()
        .map(|module| crate::pdb_symbols::discover_module(module, &capture.discovery))
        .collect()
}

/// No symbol-file parser exists for this platform yet, so every frame stays
/// `RawOnly` — the module and offset are still reported.
#[cfg(not(target_os = "windows"))]
fn build_symbol_cache(capture: &RawCapture) -> SymbolCache {
    capture
        .modules
        .iter()
        .map(|module| crate::object_symbols::discover_module(module, &capture.discovery))
        .collect()
}

/// Per-module account of the symbol lookup.
#[cfg(target_os = "windows")]
fn module_reports(capture: &RawCapture, cache: &SymbolCache) -> Vec<ModuleReport> {
    use crate::pdb_symbols::ModuleSymbols;

    capture
        .modules
        .iter()
        .zip(cache)
        .map(|(module, symbols)| {
            let (status, symbol_file, symbol_source, rejected) = match symbols {
                ModuleSymbols::Found {
                    symbol_file,
                    source,
                    ..
                } => (
                    ModuleSymbolStatus::Resolved,
                    Some(symbol_file.clone()),
                    Some(*source),
                    0,
                ),
                ModuleSymbols::NotFound => (ModuleSymbolStatus::NotFound, None, None, 0),
                ModuleSymbols::Mismatched { rejected } => {
                    (ModuleSymbolStatus::Mismatched, None, None, *rejected)
                }
                ModuleSymbols::NoDebugDirectory => {
                    (ModuleSymbolStatus::NoDebugDirectory, None, None, 0)
                }
            };
            ModuleReport {
                name: module.name.clone(),
                status,
                symbol_file,
                symbol_source,
                rejected_candidates: rejected,
            }
        })
        .collect()
}

/// No symbol reader on this platform, so every module reports as much rather
/// than as "not found", which would blame the build for a missing parser.
#[cfg(not(target_os = "windows"))]
fn module_reports(capture: &RawCapture, cache: &SymbolCache) -> Vec<ModuleReport> {
    use crate::object_symbols::ModuleSymbols;

    capture
        .modules
        .iter()
        .zip(cache)
        .map(|(module, symbols)| {
            let (status, symbol_file, symbol_source, rejected) = match symbols {
                ModuleSymbols::Found {
                    symbol_file,
                    source,
                    ..
                } => (
                    ModuleSymbolStatus::Resolved,
                    Some(symbol_file.clone()),
                    Some(*source),
                    0,
                ),
                ModuleSymbols::NotFound => (ModuleSymbolStatus::NotFound, None, None, 0),
                ModuleSymbols::Mismatched { rejected } => {
                    (ModuleSymbolStatus::Mismatched, None, None, *rejected)
                }
                ModuleSymbols::NoDebugDirectory => {
                    (ModuleSymbolStatus::NoDebugDirectory, None, None, 0)
                }
            };
            ModuleReport {
                name: module.name.clone(),
                status,
                symbol_file,
                symbol_source,
                rejected_candidates: rejected,
            }
        })
        .collect()
}

#[cfg(target_os = "windows")]
fn lookup(cache: &SymbolCache, module_index: usize, relative_address: u64) -> Option<String> {
    let crate::pdb_symbols::ModuleSymbols::Found { table, .. } = cache.get(module_index)? else {
        return None;
    };
    table.lookup(relative_address).map(str::to_owned)
}

#[cfg(not(target_os = "windows"))]
fn lookup(cache: &SymbolCache, module_index: usize, relative_address: u64) -> Option<String> {
    let crate::object_symbols::ModuleSymbols::Found { table, .. } = cache.get(module_index)? else {
        return None;
    };
    table.lookup(relative_address).map(str::to_owned)
}

/// The `(file, line)` for a frame, when line resolution was asked for (#803).
///
/// Always `None` on Windows: the `pdb` crate panics iterating line programs on
/// PDBs rustc produces, so that side needs a different crate before it can
/// answer this. Returning `None` keeps the frame honest — module + offset —
/// rather than pretending the information is unavailable for a different
/// reason.
#[cfg(not(target_os = "windows"))]
fn lookup_line(
    cache: &SymbolCache,
    module_index: usize,
    relative_address: u64,
) -> Option<(String, u32)> {
    let crate::object_symbols::ModuleSymbols::Found { lines, .. } = cache.get(module_index)? else {
        return None;
    };
    lines
        .as_ref()?
        .lookup(relative_address)
        .map(|(file, line)| (file.to_owned(), line))
}

#[cfg(target_os = "windows")]
fn lookup_line(
    _cache: &SymbolCache,
    _module_index: usize,
    _relative_address: u64,
) -> Option<(String, u32)> {
    None
}

fn symbolize_thread(capture: &RawCapture, cache: &SymbolCache, thread: &RawThread) -> SymThread {
    let frames = thread
        .frames
        .iter()
        .map(|frame| {
            match capture.modules.get(frame.module_index as usize) {
                Some(module) => {
                    let function =
                        lookup(cache, frame.module_index as usize, frame.relative_address);
                    let (file, line) = match lookup_line(
                        cache,
                        frame.module_index as usize,
                        frame.relative_address,
                    ) {
                        Some((file, line)) => (Some(file), Some(line)),
                        None => (None, None),
                    };
                    SymFrame {
                        module: module.name.clone(),
                        relative_address: frame.relative_address,
                        // Resolved only when a symbol file actually produced a
                        // name; everything else keeps module + offset and says
                        // so.
                        status: if function.is_some() {
                            FrameStatus::Resolved
                        } else {
                            FrameStatus::RawOnly
                        },
                        function,
                        file,
                        line,
                        inline_frames: Vec::new(),
                    }
                }
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
            discovery: Default::default(),
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

    /// Every module gets an account, so a reader can tell why a frame is
    /// unsymbolized rather than only that it is.
    #[test]
    fn every_module_is_accounted_for() {
        let capture = capture_with(vec![module("a.dll"), module("b.dll")], Vec::new());
        let report = symbolize(&capture).unwrap();
        assert_eq!(report.modules.len(), 2);
        assert_eq!(report.modules[0].name, "a.dll");
        assert_eq!(report.modules[1].name, "b.dll");
    }

    /// A module with no path could never be looked beside; that is a property
    /// of the capture, and must not be reported as a stripped build.
    #[test]
    fn a_module_without_a_path_is_not_found_rather_than_stripped() {
        let capture = capture_with(vec![module("ghost.dll")], Vec::new());
        let status = symbolize(&capture).unwrap().modules[0].status.clone();
        assert!(
            matches!(
                status,
                ModuleSymbolStatus::NotFound | ModuleSymbolStatus::Unsupported
            ),
            "got {status:?}"
        );
    }

    /// A missing binary yields "not found", never "mismatched": nothing was
    /// rejected, so nothing is misconfigured.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_missing_binary_reports_not_found_with_no_rejections() {
        let capture = RawCapture {
            format: CaptureFormat::CooperativeFrames,
            discovery: Default::default(),
            modules: vec![ModuleRef {
                name: "gone.dll".into(),
                path_hint: Some("no-such-binary-anywhere.dll".into()),
                ..Default::default()
            }],
            threads: Vec::new(),
        };
        let entry = symbolize(&capture).unwrap().modules.remove(0);
        assert_eq!(entry.status, ModuleSymbolStatus::NoDebugDirectory);
        assert_eq!(entry.rejected_candidates, 0);
        assert!(entry.symbol_file.is_none());
    }

    /// The status names are wire surface; renaming one silently breaks a
    /// consumer.
    #[test]
    fn module_status_uses_stable_wire_names() {
        assert_eq!(
            serde_json::to_string(&ModuleSymbolStatus::Mismatched).unwrap(),
            r#""mismatched""#
        );
        assert_eq!(
            serde_json::to_string(&ModuleSymbolStatus::NoDebugDirectory).unwrap(),
            r#""no_debug_directory""#
        );
        assert_eq!(
            serde_json::to_string(&ModuleSymbolStatus::NotFound).unwrap(),
            r#""not_found""#
        );
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
            discovery: Default::default(),
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

    /// End-to-end: a real binary, a real symbol file, a real function name.
    ///
    /// Everything above uses synthetic modules that can never resolve. This
    /// takes the test binary's own PDB, picks a symbol out of it, and feeds
    /// that symbol's address through the public `symbolize` entry point —
    /// so the module lookup, the cache, the status decision, and the PDB
    /// arithmetic all have to agree for the expected name to come back.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_real_address_resolves_to_a_real_function_name() {
        use crate::pdb_symbols::SymbolTable;

        let exe = std::env::current_exe().expect("current exe");
        let pdb = exe.with_extension("pdb");
        if !pdb.is_file() {
            // A silent skip here would report "symbolization works" while
            // symbolizing nothing; CI must say so instead.
            assert!(
                std::env::var_os("GITHUB_ACTIONS").is_none(),
                "no PDB at {} during a CI run; this test would assert nothing",
                pdb.display()
            );
            eprintln!("skipping: no PDB beside the test binary");
            return;
        }
        let Some(table) = SymbolTable::from_pdb(&pdb) else {
            eprintln!("skipping: PDB had no public function symbols");
            return;
        };
        // A symbol from this crate, so the expected name is recognizable
        // rather than an arbitrary runtime internal.
        let Some((rva, expected)) = table.symbol_containing_name("running_process_probe_worker")
        else {
            eprintln!("skipping: no symbol from this crate in the PDB");
            return;
        };

        let capture = RawCapture {
            format: CaptureFormat::CooperativeFrames,
            discovery: Default::default(),
            modules: vec![ModuleRef {
                name: "self".into(),
                path_hint: Some(exe.to_string_lossy().into_owned()),
                ..Default::default()
            }],
            threads: vec![RawThread {
                os_tid: 1,
                frames: vec![RawFrame {
                    module_index: 0,
                    relative_address: u64::from(rva),
                }],
                ..Default::default()
            }],
        };

        let frame = &symbolize(&capture).unwrap().threads[0].frames[0];
        assert_eq!(frame.status, FrameStatus::Resolved);
        assert_eq!(frame.function.as_deref(), Some(expected.as_str()));
        assert_eq!(
            frame.relative_address,
            u64::from(rva),
            "the offset must survive symbolization"
        );
    }

    /// A module with no symbol file must degrade, not fail.
    #[test]
    fn a_module_without_symbols_stays_raw_only() {
        let capture = RawCapture {
            format: CaptureFormat::CooperativeFrames,
            discovery: Default::default(),
            modules: vec![ModuleRef {
                name: "ghost.dll".into(),
                path_hint: Some("no-such-binary-anywhere.dll".into()),
                ..Default::default()
            }],
            threads: vec![RawThread {
                os_tid: 1,
                frames: vec![RawFrame {
                    module_index: 0,
                    relative_address: 0x40,
                }],
                ..Default::default()
            }],
        };
        let frame = &symbolize(&capture).unwrap().threads[0].frames[0];
        assert_eq!(frame.status, FrameStatus::RawOnly);
        assert!(frame.function.is_none(), "no symbols means no name");
        assert_eq!(frame.relative_address, 0x40);
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
