//! A real jemalloc dump, parsed by the real parser (#788, follow-through on #646).
//!
//! The unit tests in `src/profile/heap/tests.rs` drive the parser from a
//! hand-authored sample. That proved the lowering and the exports, but not
//! that the sample resembled anything jemalloc emits — and it did not: the
//! parser read the gperftools one-line layout while every jemalloc it can be
//! pointed at writes `heap_v2`, which splits counts from addresses and repeats
//! each stack per thread. Against a real dump the parser returned zero stacks:
//! precisely the "empty profile that looks like this program allocates
//! nothing" the module documents as the outcome it will never produce.
//!
//! So this test runs a fixture that really allocates, really asks jemalloc for
//! a dump, and parses the file jemalloc really wrote.
//!
//! # What it asserts, and why not function names
//!
//! The daemon's default resolver attributes an address to a module and an
//! offset (`binary+0x1234`) and deliberately stops there — real names come
//! from the out-of-process symbolizer, which is a separate tier this test does
//! not reach into. So "the leaky call site dominates" is asserted as: one
//! stack holds nearly all live bytes, and it attributes to the fixture binary.
//! That is the strongest claim available without the symbol tier, and it still
//! fails if the parser regresses.

use std::path::PathBuf;
use std::process::Command;

use running_process_probe_daemon::profile::heap;
use running_process_probe_daemon::profile::symbolize::{Frame, FrameResolver};

/// Blocks allocated by the fixture, and their size. Mirrors `BLOCKS` in
/// `testbins/src/bin/jemalloc_leaker.rs`; the point of the test is that the
/// bytes jemalloc reports match what the fixture actually held.
const EXPECTED_BLOCKS: i64 = 2000;
const BLOCK_BYTES: i64 = 4096;
const EXPECTED_BYTES: i64 = EXPECTED_BLOCKS * BLOCK_BYTES;

/// Locate a fixture binary built by `soldr cargo build -p testbins`.
fn testbin_path(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    let profile_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    let path = profile_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "test fixture `{name}` is missing at {}.\n\
         Build the fixtures first:  soldr cargo build -p testbins",
        path.display()
    );
    path
}

/// Run the fixture with profiling on and return the dump it wrote.
///
/// `None` means this platform cannot host the fixture, which the callers
/// report as a skip. A platform gap is not a regression in the parser.
fn capture_dump() -> Option<(String, PathBuf)> {
    let fixture = testbin_path("testbin-jemalloc-leaker");
    let dir = std::env::temp_dir().join(format!("rp-heap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let dump = dir.join("heap.dump");

    // `MALLOC_CONF`, not `_RJEM_MALLOC_CONF`: the fixture enables
    // `unprefixed_malloc_on_supported_platforms`, and a build with unprefixed
    // symbols reads the unprefixed variable. Setting the other one is silently
    // ignored — which is exactly the trap the `ProfilingDisabled` remediation
    // text used to walk operators into.
    //
    // `lg_prof_sample:0` samples every allocation, so the fixture's blocks are
    // all accounted for rather than statistically estimated.
    let output = Command::new(&fixture)
        .arg(&dump)
        .env("MALLOC_CONF", "prof:true,lg_prof_sample:0")
        .output()
        .expect("run jemalloc fixture");

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if let Some(reason) = stdout.strip_prefix("NO_JEMALLOC ") {
        eprintln!("skipping: {reason}");
        return None;
    }
    if stdout == "PROFILING_OFF" {
        eprintln!("skipping: jemalloc is linked but refused to enable profiling here");
        return None;
    }
    assert!(
        stdout.starts_with("DUMPED "),
        "fixture did not report a dump: stdout={stdout:?} stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    let text = std::fs::read_to_string(&dump)
        .unwrap_or_else(|e| panic!("read dump at {}: {e}", dump.display()));
    Some((text, fixture))
}

/// Names every address for the module it falls in, so a test can ask which
/// binary a stack belongs to without the symbol tier.
struct MappingResolver {
    mappings: Vec<heap::HeapMapping>,
}

impl FrameResolver for MappingResolver {
    fn resolve(&mut self, address: u64) -> Frame {
        let module = self
            .mappings
            .iter()
            .find(|m| address >= m.start && address < m.end)
            .map(|m| {
                std::path::Path::new(&m.path)
                    .file_name()
                    .map(|leaf| leaf.to_string_lossy().into_owned())
                    .unwrap_or_else(|| m.path.clone())
            })
            .unwrap_or_default();
        Frame {
            function: if module.is_empty() {
                format!("0x{address:x}")
            } else {
                module.clone()
            },
            module,
            relative_address: address,
        }
    }
}

#[test]
fn a_real_jemalloc_dump_parses_into_stacks() {
    let Some((text, _fixture)) = capture_dump() else {
        return;
    };

    // Guard the premise: if jemalloc ever switches formats again, say so here
    // rather than through a confusing count assertion below.
    assert!(
        text.starts_with("heap_v2/"),
        "expected a heap_v2 dump, got: {:?}",
        text.lines().next()
    );

    let profile = heap::parse_jeprof(&text);
    assert!(
        !profile.stacks.is_empty(),
        "a real dump parsed to zero stacks — the parser cannot read what \
         jemalloc emits"
    );
    assert!(
        !profile.mappings.is_empty(),
        "MAPPED_LIBRARIES did not parse, so nothing could be symbolized"
    );
}

#[test]
fn the_live_bytes_match_what_the_fixture_held_and_are_not_double_counted() {
    let Some((text, _fixture)) = capture_dump() else {
        return;
    };
    let profile = heap::parse_jeprof(&text);
    let live: i64 = profile.stacks.iter().map(|s| s.inuse_bytes).sum();

    // The fixture's own blocks plus whatever the runtime holds — never less
    // than what it allocated, and never near double it. Double is the specific
    // failure this pins: `heap_v2` repeats every stack as `t*` (all threads)
    // and `tN` (per thread), so summing both counts each byte twice.
    assert!(
        live >= EXPECTED_BYTES,
        "live bytes {live} is below the {EXPECTED_BYTES} the fixture held"
    );
    assert!(
        live < EXPECTED_BYTES * 2,
        "live bytes {live} is at least double the {EXPECTED_BYTES} the fixture \
         held — per-thread `tN` lines are being summed alongside their `t*` total"
    );
}

#[test]
fn the_leaking_call_site_dominates_the_flame_graph() {
    let Some((text, fixture)) = capture_dump() else {
        return;
    };
    let profile = heap::parse_jeprof(&text);
    let mut resolver = MappingResolver {
        mappings: profile.mappings.clone(),
    };
    let collapsed = heap::to_collapsed(&profile, &mut resolver);

    let top = collapsed
        .lines()
        .next()
        .expect("collapsed output should have at least one row");
    let (stack, bytes) = top
        .rsplit_once(' ')
        .expect("collapsed row is `stack bytes`");
    let bytes: i64 = bytes.parse().expect("row weight is an integer");

    // `to_collapsed` sorts by weight, so the first row is the heaviest stack.
    // The fixture allocates one big thing and little else, so that row should
    // be the fixture's own allocation and should carry essentially all of it.
    assert!(
        bytes >= EXPECTED_BYTES,
        "the heaviest stack holds {bytes} bytes, less than the {EXPECTED_BYTES} \
         the fixture allocated at one call site: {stack}"
    );

    let binary = fixture
        .file_name()
        .expect("fixture file name")
        .to_string_lossy()
        .into_owned();
    assert!(
        stack.contains(&binary),
        "the heaviest stack does not attribute to the fixture binary \
         ({binary}): {stack}"
    );
}
