//! Heap profiles: jemalloc's jeprof text, lowered to pprof (S17 / #646).
//!
//! # Doubly opt-in, and structurally so
//!
//! A heap profile needs two things the daemon cannot arrange:
//!
//! 1. The application built with jemalloc as its global allocator, with
//!    profiling compiled in.
//! 2. `prof:true` set in the allocator's config at process start.
//!
//! Neither can be turned on from outside, which is the point: continuous
//! allocation profiling costs the target real throughput, so it is the
//! application's decision, made before it starts, not something an operator
//! can impose on a running process.
//!
//! When either is missing the answer is a typed [`HeapUnavailable`] naming
//! which one and how to fix it — never a crash, and never an empty profile
//! that looks like "this program allocates nothing".
//!
//! # What the daemon does and does not do
//!
//! The target calls jemalloc's own `prof.dump`, which writes a file, and hands
//! back the path. The daemon never allocates in the target and never runs
//! jemalloc itself. Parsing and symbolization happen here, afterwards — the
//! same division as CPU profiling, for the same reason.
//!
//! # The formats — there are two, and jemalloc emits the second
//!
//! The gperftools-style layout puts a stack's counts and its addresses on one
//! line:
//!
//! ```text
//! heap profile:    <inuse_objs>: <inuse_bytes> [<alloc_objs>: <alloc_bytes>] @ heapprofile
//!   <objs>: <bytes> [<objs>: <bytes>] @ 0x7f1a 0x7f2b ...
//! ```
//!
//! Every jemalloc this crate can actually be pointed at writes `heap_v2`
//! instead, which splits them across lines and repeats each stack once per
//! thread:
//!
//! ```text
//! heap_v2/<sample_interval>
//!   t*: <objs>: <bytes> [<objs>: <bytes>]      <- process-wide totals
//! @ 0x7f1a 0x7f2b ...
//!   t*: <objs>: <bytes> [<objs>: <bytes>]      <- this stack, all threads
//!   t0: <objs>: <bytes> [<objs>: <bytes>]      <- this stack, thread 0
//! MAPPED_LIBRARIES:
//!   7f1a0000-7f1b0000 r-xp 00000000 08:01 1234  /usr/lib/libfoo.so
//! ```
//!
//! Both are parsed. Two properties of `heap_v2` matter and are easy to get
//! wrong:
//!
//! - The `t*` line is the sum over threads and the `tN` lines are its parts,
//!   so counting both double-counts every byte. Only `t*` is taken.
//! - The leading `t*` before the first `@` is the process-wide total, the same
//!   role the `heap profile:` header plays above, so it is skipped for the
//!   same reason.
//!
//! The `MAPPED_LIBRARIES` block is `/proc/self/maps`, which is what lets the
//! addresses be symbolized later against the right binaries.

use std::path::PathBuf;

use crate::profile::export::pprof::HeapProfileBuilder;

/// Why a heap profile could not be produced.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HeapUnavailable {
    /// The target is not a jemalloc build.
    #[error(
        "this process does not use jemalloc, so it has no heap profiler.\n\
         Rebuild with:\n  \
         tikv-jemallocator = {{ version = \"0.6\", features = [\"profiling\", \
         \"unprefixed_malloc_on_supported_platforms\"] }}\n  \
         #[global_allocator] static A: tikv_jemallocator::Jemalloc = \
         tikv_jemallocator::Jemalloc;"
    )]
    NotJemalloc,
    /// jemalloc is present but profiling was not enabled at startup.
    ///
    /// The variable to set depends on whether the build prefixes jemalloc's
    /// symbols. With `unprefixed_malloc_on_supported_platforms` — which
    /// [`NotJemalloc`] tells you to enable — it is plain `MALLOC_CONF`;
    /// `_RJEM_MALLOC_CONF` is silently ignored by such a build, so naming only
    /// the prefixed form sends the operator to set a variable that does
    /// nothing and read the same error again.
    ///
    /// [`NotJemalloc`]: HeapUnavailable::NotJemalloc
    #[error(
        "jemalloc is present but profiling is off. It can only be enabled at \
         process start, not from here.\n\
         Restart the target with:\n  MALLOC_CONF=prof:true\n\
         (or _RJEM_MALLOC_CONF=prof:true if the build keeps jemalloc's \
         `_rjem_` symbol prefix — i.e. without the \
         `unprefixed_malloc_on_supported_platforms` feature.)"
    )]
    ProfilingDisabled,
    /// This platform has no jemalloc build.
    #[error(
        "heap profiling is not available on {os}: jemalloc is not supported \
         there. CPU profiling works on every platform."
    )]
    UnsupportedPlatform {
        /// The platform that cannot do this.
        os: &'static str,
    },
    /// The dump was requested but could not be read or parsed.
    #[error("the heap dump at {path} could not be read: {detail}")]
    Unreadable {
        /// Where the dump was expected.
        path: PathBuf,
        /// What went wrong.
        detail: String,
    },
}

/// One folded heap stack: four counts and its addresses, leaf first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeapStack {
    /// Objects allocated over the process's life.
    pub alloc_objects: i64,
    /// Bytes allocated over the process's life.
    pub alloc_bytes: i64,
    /// Objects still live.
    pub inuse_objects: i64,
    /// Bytes still live.
    pub inuse_bytes: i64,
    /// Return addresses, leaf first.
    pub addresses: Vec<u64>,
}

/// One mapped module, from the `MAPPED_LIBRARIES` block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeapMapping {
    /// First mapped address.
    pub start: u64,
    /// One past the last mapped address.
    pub end: u64,
    /// Offset into the file.
    pub offset: u64,
    /// Path of the mapped file.
    pub path: String,
}

/// A parsed jeprof dump.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeapProfile {
    /// Per-stack counts.
    pub stacks: Vec<HeapStack>,
    /// Modules, so the addresses can be symbolized against the right binaries.
    pub mappings: Vec<HeapMapping>,
}

/// Parse jemalloc's jeprof text.
///
/// Malformed stack lines are skipped rather than failing the parse. A heap
/// dump is a snapshot of a live process and can be truncated if the process
/// exits mid-write; salvaging the readable part beats discarding a profile
/// that is mostly intact.
pub fn parse_jeprof(text: &str) -> HeapProfile {
    let mut profile = HeapProfile::default();
    let mut in_mappings = false;
    // Addresses from the most recent `@` line, waiting for the `t*` counts
    // that belong to them. `None` means we are before the first `@`, where a
    // `t*` line is the process-wide total rather than any one stack's.
    let mut pending: Option<Vec<u64>> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("MAPPED_LIBRARIES") {
            in_mappings = true;
            continue;
        }
        if in_mappings {
            if let Some(mapping) = parse_mapping(trimmed) {
                profile.mappings.push(mapping);
            }
            continue;
        }
        // The header carries process-wide totals that are the sum of the
        // stacks below it, so folding it in as well would double every number.
        if trimmed.starts_with("heap profile:") || trimmed.starts_with("heap_v2") {
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }

        // `heap_v2`: an address-only line opens a stack whose counts follow.
        if let Some(addresses) = trimmed.strip_prefix('@') {
            let addresses = parse_addresses(addresses);
            pending = (!addresses.is_empty()).then_some(addresses);
            continue;
        }
        // `heap_v2`: the counts for the stack opened above. Only `t*` — the
        // per-thread `tN` lines are its components, and adding them too would
        // double every number in the profile.
        //
        // Two things enforce that, and it is worth being precise about which:
        // matching `t*` exactly keeps `tN` out of here, and `take()` (not
        // `clone()`) means even a repeated `t*` cannot be attributed twice.
        // The `tN` arm below is for clarity, not protection — a `tN` line that
        // reached the bottom of this loop would be rejected anyway for having
        // no `@`.
        if let Some(counts) = trimmed.strip_prefix("t*:") {
            let Some(addresses) = pending.take() else {
                // Before the first `@`: process-wide totals, not a stack.
                continue;
            };
            if let Some(stack) = parse_counts(counts, addresses) {
                profile.stacks.push(stack);
            }
            continue;
        }
        if trimmed.starts_with('t') {
            // A per-thread `tN` line: accounted for by the `t*` above it.
            // Explicit rather than left to fall through, so the intent does
            // not depend on how strict `parse_stack` happens to be.
            continue;
        }

        // gperftools-style: counts and addresses share one line.
        if let Some(stack) = parse_stack(trimmed) {
            profile.stacks.push(stack);
        }
    }
    profile
}

/// Parse a whitespace-separated list of `0x`-prefixed return addresses.
fn parse_addresses(text: &str) -> Vec<u64> {
    text.split_whitespace()
        .filter_map(|token| u64::from_str_radix(token.trim_start_matches("0x"), 16).ok())
        .collect()
}

/// Parse ` <objs>: <bytes> [<objs>: <bytes>]` against an already-read stack.
fn parse_counts(counts: &str, addresses: Vec<u64>) -> Option<HeapStack> {
    let (inuse, alloc) = counts.split_once('[')?;
    let (inuse_objects, inuse_bytes) = parse_pair(inuse)?;
    let (alloc_objects, alloc_bytes) = parse_pair(alloc.trim_end_matches([']', ' ']))?;
    Some(HeapStack {
        alloc_objects,
        alloc_bytes,
        inuse_objects,
        inuse_bytes,
        addresses,
    })
}

/// Parse `<objs>: <bytes> [<objs>: <bytes>] @ 0xa 0xb`.
fn parse_stack(line: &str) -> Option<HeapStack> {
    let (counts, addresses) = line.split_once('@')?;
    let addresses = parse_addresses(addresses);
    if addresses.is_empty() {
        return None;
    }
    parse_counts(counts, addresses)
}

/// Parse `<objs>: <bytes>`.
fn parse_pair(text: &str) -> Option<(i64, i64)> {
    let (objects, bytes) = text.trim().split_once(':')?;
    Some((objects.trim().parse().ok()?, bytes.trim().parse().ok()?))
}

/// Parse one `/proc/self/maps` line.
fn parse_mapping(line: &str) -> Option<HeapMapping> {
    let mut fields = line.split_whitespace();
    let range = fields.next()?;
    let _perms = fields.next()?;
    let offset = fields.next()?;
    let (start, end) = range.split_once('-')?;

    // Anonymous mappings have no path. They cannot be symbolized, so keeping
    // them would only add rows a reader has to skip.
    let path = fields.nth(2)?;
    if !path.starts_with('/') && !path.contains(':') {
        return None;
    }

    Some(HeapMapping {
        start: u64::from_str_radix(start, 16).ok()?,
        end: u64::from_str_radix(end, 16).ok()?,
        offset: u64::from_str_radix(offset, 16).ok()?,
        path: path.to_string(),
    })
}

/// Lower a parsed heap profile to pprof.
///
/// Four sample types, because "what is leaking" and "what is churning" are
/// different questions and a single number answers neither well:
///
/// | type | meaning |
/// |---|---|
/// | `alloc_objects` / `alloc_space` | everything ever allocated — churn |
/// | `inuse_objects` / `inuse_space` | still live — leaks |
///
/// `inuse_space` is the default, so a flame graph opens on live bytes, which
/// is what someone chasing a leak came for.
pub fn to_pprof(
    profile: &HeapProfile,
    resolver: &mut dyn crate::profile::FrameResolver,
) -> Vec<u8> {
    let mut builder = HeapProfileBuilder::new();
    for mapping in &profile.mappings {
        builder.add_mapping(mapping.start, mapping.end, mapping.offset, &mapping.path);
    }
    for stack in &profile.stacks {
        let frames: Vec<String> = stack
            .addresses
            .iter()
            .map(|address| resolver.resolve(*address).function)
            .collect();
        builder.add_sample(
            &frames,
            [
                stack.alloc_objects,
                stack.alloc_bytes,
                stack.inuse_objects,
                stack.inuse_bytes,
            ],
        );
    }
    builder.finish()
}

/// Render a heap profile as collapsed stacks, weighted by live bytes.
///
/// Feeds the same flame graph as CPU. Live bytes rather than allocation count
/// because a leak is measured in memory, and one enormous allocation matters
/// more than a million small freed ones.
pub fn to_collapsed(
    profile: &HeapProfile,
    resolver: &mut dyn crate::profile::FrameResolver,
) -> String {
    use std::collections::BTreeMap;
    let mut folded: BTreeMap<String, i64> = BTreeMap::new();
    for stack in &profile.stacks {
        if stack.inuse_bytes <= 0 {
            continue;
        }
        // Root-first, and semicolons in a name replaced: the collapsed format
        // has no escape syntax, so one would forge a frame.
        let frames: Vec<String> = stack
            .addresses
            .iter()
            .rev()
            .map(|address| resolver.resolve(*address).function.replace(';', ":"))
            .collect();
        *folded.entry(frames.join(";")).or_insert(0) += stack.inuse_bytes;
    }

    let mut rows: Vec<(String, i64)> = folded.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.into_iter()
        .map(|(stack, bytes)| format!("{stack} {bytes}\n"))
        .collect()
}

/// Whether this platform can host a jemalloc heap profiler at all.
///
/// Checked before anything else so the operator is told "not on this OS"
/// rather than "your build is wrong", which would send them to rebuild
/// something that was never going to work.
pub fn platform_supported() -> Result<(), HeapUnavailable> {
    if cfg!(target_os = "windows") {
        return Err(HeapUnavailable::UnsupportedPlatform { os: "windows" });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
