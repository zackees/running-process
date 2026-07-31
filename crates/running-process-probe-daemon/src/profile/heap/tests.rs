//! Tests for jeprof parsing and pprof lowering (#646).

use prost::Message as _;

use super::*;
use crate::profile::pprof::Profile;
use crate::profile::symbolize::TableResolver;

/// A dump in the gperftools one-line layout.
///
/// This was once described here as "jemalloc's real output shape". It is not —
/// jemalloc writes [`HEAP_V2`], and the difference is why a real dump parsed to
/// zero stacks until #788. It is kept because the layout is a real one and
/// costs nothing to accept, but the end-to-end coverage lives in
/// `tests/profile_heap_test.rs`, against a dump jemalloc actually wrote.
///
/// Two stacks with different alloc/inuse splits, so a test can tell the four
/// value columns apart — one that churned and freed, one that is still live.
const DUMP: &str = "\
heap profile:     12:     3072 [    40:    10240] @ heapprofile
     10:     2048 [    10:     2048] @ 0x7f1a0100 0x7f1a0200
      2:     1024 [    30:     8192] @ 0x7f1b0100 0x7f1a0200

MAPPED_LIBRARIES:
7f1a0000-7f1a1000 r-xp 00000000 08:01 100    /usr/lib/libleaky.so
7f1b0000-7f1b1000 r-xp 00001000 08:01 101    /usr/lib/libchurny.so
7ffd0000-7ffd1000 rw-p 00000000 00:00 0
";

/// The same two stacks as [`DUMP`], in the `heap_v2` layout jemalloc emits.
///
/// Trimmed from a dump written by `testbin-jemalloc-leaker` under
/// `MALLOC_CONF=prof:true`, with the addresses swapped for the ones
/// [`resolver`] knows so both layouts can be asserted to parse identically.
///
/// Note the shape: counts follow their `@` line rather than sharing it, the
/// leading `t*` is a process-wide total belonging to no stack, and each stack
/// appears twice — once as `t*` (all threads) and once per thread as `tN`.
const HEAP_V2: &str = "\
heap_v2/1
  t*: 12: 3072 [40: 10240]
@ 0x7f1a0100 0x7f1a0200
  t*: 10: 2048 [10: 2048]
  t0: 10: 2048 [10: 2048]
@ 0x7f1b0100 0x7f1a0200
  t*: 2: 1024 [30: 8192]
  t0: 2: 1024 [30: 8192]

MAPPED_LIBRARIES:
7f1a0000-7f1a1000 r-xp 00000000 08:01 100    /usr/lib/libleaky.so
7f1b0000-7f1b1000 r-xp 00001000 08:01 101    /usr/lib/libchurny.so
7ffd0000-7ffd1000 rw-p 00000000 00:00 0
";

fn resolver() -> TableResolver {
    TableResolver::default()
        .with(0x7f1a0100, "leak_here")
        .with(0x7f1a0200, "main")
        .with(0x7f1b0100, "churn_here")
}

#[test]
fn each_stack_keeps_its_four_counts_and_its_addresses() {
    let profile = parse_jeprof(DUMP);
    assert_eq!(profile.stacks.len(), 2);

    let leaky = &profile.stacks[0];
    assert_eq!(leaky.inuse_objects, 10);
    assert_eq!(leaky.inuse_bytes, 2048);
    assert_eq!(leaky.alloc_objects, 10);
    assert_eq!(leaky.alloc_bytes, 2048);
    // Leaf first, as jemalloc writes them.
    assert_eq!(leaky.addresses, vec![0x7f1a0100, 0x7f1a0200]);

    // The churny one allocated far more than it kept, which is the whole
    // reason alloc_* and inuse_* are separate columns.
    let churny = &profile.stacks[1];
    assert_eq!(churny.inuse_bytes, 1024);
    assert_eq!(churny.alloc_bytes, 8192);
}

#[test]
fn the_header_totals_are_not_folded_in_as_a_stack() {
    // They are the sum of the stacks below, so counting them too would double
    // every number in the profile.
    let profile = parse_jeprof(DUMP);
    assert_eq!(profile.stacks.len(), 2);
    assert_eq!(
        profile.stacks.iter().map(|s| s.inuse_bytes).sum::<i64>(),
        3072,
        "the header's 3072 must equal the stacks, not be added to them"
    );
}

#[test]
fn named_mappings_are_kept_and_anonymous_ones_are_not() {
    let profile = parse_jeprof(DUMP);
    assert_eq!(profile.mappings.len(), 2);
    assert_eq!(profile.mappings[0].start, 0x7f1a0000);
    assert_eq!(profile.mappings[0].end, 0x7f1a1000);
    assert_eq!(profile.mappings[0].path, "/usr/lib/libleaky.so");
    assert_eq!(profile.mappings[1].offset, 0x1000);
    // An anonymous mapping cannot be symbolized, so keeping it would only add
    // a row the reader has to skip.
    assert!(profile.mappings.iter().all(|m| m.path.starts_with('/')));
}

#[test]
fn a_truncated_dump_yields_the_stacks_it_did_contain() {
    // A heap dump is written from a live process and can be cut off if that
    // process exits mid-write. Salvaging the readable part beats discarding a
    // profile that is mostly intact.
    let truncated = "\
heap profile:     12:     3072 [    40:    10240] @ heapprofile
     10:     2048 [    10:     2048] @ 0x7f1a0100 0x7f1a0200
      2:  10";
    let profile = parse_jeprof(truncated);
    assert_eq!(profile.stacks.len(), 1);
    assert_eq!(profile.stacks[0].inuse_bytes, 2048);
}

#[test]
fn a_stack_line_with_no_addresses_is_skipped() {
    let profile = parse_jeprof("     1:     8 [     1:     8] @ \n");
    assert!(profile.stacks.is_empty());
}

#[test]
fn an_empty_dump_parses_to_an_empty_profile_rather_than_failing() {
    assert_eq!(parse_jeprof(""), HeapProfile::default());
}

// --- pprof lowering -------------------------------------------------------

#[test]
fn the_pprof_carries_all_four_heap_sample_types() {
    let profile = parse_jeprof(DUMP);
    let bytes = to_pprof(&profile, &mut resolver());
    let decoded = Profile::decode(bytes.as_slice()).expect("heap pprof must decode");

    let names: Vec<&str> = decoded
        .sample_type
        .iter()
        .map(|t| decoded.string_table[t.r#type as usize].as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "alloc_objects",
            "alloc_space",
            "inuse_objects",
            "inuse_space"
        ]
    );

    let units: Vec<&str> = decoded
        .sample_type
        .iter()
        .map(|t| decoded.string_table[t.unit as usize].as_str())
        .collect();
    assert_eq!(units, vec!["count", "bytes", "count", "bytes"]);
}

#[test]
fn the_default_view_is_live_bytes() {
    // Someone opening a heap profile is usually chasing a leak, so the graph
    // should open on what is still held rather than on total churn.
    let bytes = to_pprof(&parse_jeprof(DUMP), &mut resolver());
    let decoded = Profile::decode(bytes.as_slice()).expect("decode");
    assert_eq!(
        decoded.string_table[decoded.default_sample_type as usize],
        "inuse_space"
    );
}

#[test]
fn sample_values_are_in_declared_order() {
    let bytes = to_pprof(&parse_jeprof(DUMP), &mut resolver());
    let decoded = Profile::decode(bytes.as_slice()).expect("decode");
    // The churny stack: 30 objects / 8192 bytes allocated, 2 / 1024 still live.
    let churny = &decoded.sample[1];
    assert_eq!(churny.value, vec![30, 8192, 2, 1024]);
}

#[test]
fn the_mapping_table_survives_so_addresses_can_be_symbolized_later() {
    let bytes = to_pprof(&parse_jeprof(DUMP), &mut resolver());
    let decoded = Profile::decode(bytes.as_slice()).expect("decode");
    assert_eq!(decoded.mapping.len(), 2);
    let names: Vec<&str> = decoded
        .mapping
        .iter()
        .map(|m| decoded.string_table[m.filename as usize].as_str())
        .collect();
    assert!(names.contains(&"/usr/lib/libleaky.so"));
}

#[test]
fn the_string_table_still_starts_with_the_empty_string() {
    // The pprof spec invariant, and 0 is also how every optional string field
    // says "unset".
    let bytes = to_pprof(&parse_jeprof(DUMP), &mut resolver());
    let decoded = Profile::decode(bytes.as_slice()).expect("decode");
    assert_eq!(decoded.string_table[0], "");
}

#[test]
fn frames_are_symbolized_through_the_resolver() {
    let bytes = to_pprof(&parse_jeprof(DUMP), &mut resolver());
    let decoded = Profile::decode(bytes.as_slice()).expect("decode");
    let names: Vec<&str> = decoded
        .function
        .iter()
        .map(|f| decoded.string_table[f.name as usize].as_str())
        .collect();
    assert!(names.contains(&"leak_here"));
    assert!(names.contains(&"main"));
}

// --- collapsed / flame graph ---------------------------------------------

#[test]
fn collapsed_output_is_weighted_by_live_bytes_and_rooted_correctly() {
    let text = to_collapsed(&parse_jeprof(DUMP), &mut resolver());
    let lines: Vec<&str> = text.lines().collect();
    // Root first, hottest first, weighted by what is still held.
    assert_eq!(lines[0], "main;leak_here 2048");
    assert_eq!(lines[1], "main;churn_here 1024");
}

#[test]
fn a_fully_freed_stack_is_not_drawn_in_the_live_view() {
    // It allocated and gave it all back; showing it in a leak view would put
    // a wide box where there is no retained memory at all.
    let dump = "\
heap profile:      0:        0 [    99:    99000] @ heapprofile
      0:        0 [    99:    99000] @ 0x7f1a0100 0x7f1a0200
";
    assert_eq!(to_collapsed(&parse_jeprof(dump), &mut resolver()), "");
}

#[test]
fn a_semicolon_in_a_frame_name_cannot_forge_a_frame() {
    let dump = "\
heap profile:      1:        8 [     1:        8] @ heapprofile
      1:        8 [     1:        8] @ 0x99
";
    let mut resolver = TableResolver::default().with(0x99, "evil;injected");
    let text = to_collapsed(&parse_jeprof(dump), &mut resolver);
    assert!(text.contains("evil:injected"));
    assert!(!text.contains("evil;injected"));
}

// --- unavailability -------------------------------------------------------

#[test]
fn every_unavailable_reason_says_what_to_do_about_it() {
    // An empty profile that looks like "this program allocates nothing" is the
    // failure mode these exist to prevent, so each has to be actionable.
    let cases = [
        HeapUnavailable::NotJemalloc,
        HeapUnavailable::ProfilingDisabled,
        HeapUnavailable::UnsupportedPlatform { os: "windows" },
    ];
    for case in cases {
        let text = case.to_string();
        assert!(text.len() > 40, "unhelpfully terse: {text}");
    }

    assert!(HeapUnavailable::NotJemalloc
        .to_string()
        .contains("tikv-jemallocator"));
    // Profiling can only be enabled at process start, so the remedy is a
    // restart — saying so avoids an operator hunting for a runtime toggle.
    let disabled = HeapUnavailable::ProfilingDisabled.to_string();
    assert!(disabled.contains("Restart"));
    // The unprefixed variable comes first because `NotJemalloc` above tells
    // the operator to build with `unprefixed_malloc_on_supported_platforms`,
    // and such a build ignores `_RJEM_MALLOC_CONF` entirely. Naming only the
    // prefixed form — as this did until #788 — sends them to set a variable
    // that does nothing and hit the identical error again.
    assert!(disabled.contains("MALLOC_CONF=prof:true"));
    assert!(disabled.contains("_RJEM_MALLOC_CONF=prof:true"));
    assert!(
        disabled.find("MALLOC_CONF=prof:true") < disabled.find("_RJEM_MALLOC_CONF=prof:true"),
        "the variable that works with the recommended build should be named \
         first: {disabled}"
    );
}

#[test]
fn an_unsupported_platform_is_reported_before_a_build_problem() {
    // Otherwise the operator is sent to rebuild with an allocator that was
    // never going to work on their OS.
    let result = platform_supported();
    if cfg!(target_os = "windows") {
        assert_eq!(
            result,
            Err(HeapUnavailable::UnsupportedPlatform { os: "windows" })
        );
        assert!(HeapUnavailable::UnsupportedPlatform { os: "windows" }
            .to_string()
            .contains("CPU profiling works on every platform"));
    } else {
        assert!(result.is_ok());
    }
}

#[test]
fn the_layout_jemalloc_actually_emits_parses_into_the_same_stacks() {
    // This is the regression #788 exists for: against a real dump the parser
    // returned an empty profile, which reads as "this program allocates
    // nothing" rather than as a parse failure.
    let v2 = parse_jeprof(HEAP_V2);
    let legacy = parse_jeprof(DUMP);
    assert_eq!(
        v2, legacy,
        "the two layouts describe the same profile and should parse alike"
    );
    assert_eq!(v2.stacks.len(), 2);
}

#[test]
fn a_per_thread_line_is_not_counted_alongside_its_all_thread_total() {
    // `t*` is the sum over threads and `tN` are its parts. Counting both is
    // the easy mistake, and it silently doubles every byte in the profile.
    //
    // Verified by sabotage: accepting `t0:` alongside `t*:` and holding the
    // addresses instead of consuming them makes this fail, along with the
    // matching end-to-end assertion against a real dump.
    let profile = parse_jeprof(HEAP_V2);
    let live: i64 = profile.stacks.iter().map(|s| s.inuse_bytes).sum();
    assert_eq!(live, 2048 + 1024);
}

#[test]
fn the_heap_v2_process_totals_are_not_folded_in_as_a_stack() {
    // The `t*` line before the first `@` is the process-wide total, the same
    // role `heap profile:` plays in the other layout.
    let profile = parse_jeprof(HEAP_V2);
    assert!(profile.stacks.iter().all(|s| s.inuse_bytes != 3072));
}

#[test]
fn a_thread_count_line_with_no_stack_before_it_is_ignored_not_misattributed() {
    // A truncated dump can begin mid-record. Attaching those counts to
    // whatever stack came later would invent a profile, so they are dropped.
    let profile = parse_jeprof("heap_v2/1\n  t*: 9: 99 [9: 99]\n");
    assert!(profile.stacks.is_empty());
}

#[test]
fn an_address_line_with_no_counts_contributes_nothing() {
    // The other truncation: a dump cut off after its `@` line.
    let profile = parse_jeprof("heap_v2/1\n@ 0x7f1a0100 0x7f1a0200\n");
    assert!(profile.stacks.is_empty());
}
