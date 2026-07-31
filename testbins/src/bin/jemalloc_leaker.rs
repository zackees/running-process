//! Allocates at a known frame, then makes jemalloc write a heap dump (#788).
//!
//! Usage: `testbin-jemalloc-leaker <dump-path>`
//!
//! Prints one status line to stdout so the test can tell the three outcomes
//! apart without guessing from an absent file:
//!
//! - `DUMPED <path>`      — the dump was written and is ready to parse.
//! - `PROFILING_OFF`      — jemalloc is linked but `prof:true` was not set.
//! - `NO_JEMALLOC <why>`  — this build has no jemalloc at all.
//!
//! Used by `crates/running-process-probe-daemon/tests/profile_heap_test.rs`,
//! which exists because the heap parser was written against a hand-authored
//! sample and could not read what jemalloc actually emits.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// How much to allocate and hold, in 4 KiB blocks.
///
/// Large enough to dominate any incidental allocation in a fixture this small,
/// so the test can assert this frame is the top of the profile rather than
/// merely present in it.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const BLOCKS: usize = 2000;

/// The frame the test looks for. Never inlined: the whole point is that this
/// name appears in the dump's stacks.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[inline(never)]
fn leak_here(blocks: usize) -> Vec<Box<[u8; 4096]>> {
    let mut held = Vec::with_capacity(blocks);
    for _ in 0..blocks {
        held.push(Box::new([7u8; 4096]));
    }
    held
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn main() {
    let path = match std::env::args().nth(1) {
        Some(path) => path,
        None => {
            eprintln!("usage: testbin-jemalloc-leaker <dump-path>");
            std::process::exit(2);
        }
    };

    match tikv_jemalloc_ctl::profiling::prof::read() {
        Ok(true) => {}
        Ok(false) => {
            println!("PROFILING_OFF");
            return;
        }
        Err(e) => {
            println!("NO_JEMALLOC {e}");
            return;
        }
    }

    // Held across the dump: jemalloc reports what is live, so releasing this
    // first would produce a dump that correctly shows nothing leaking.
    let held = leak_here(BLOCKS);

    // `write_str` requires a 'static NUL-terminated value because jemalloc may
    // retain the pointer. Leaking one path in a fixture that exits moments
    // later is cheaper than arranging a lifetime that outlives the allocator.
    let with_nul: &'static [u8] = Box::leak(format!("{path}\0").into_bytes().into_boxed_slice());
    match tikv_jemalloc_ctl::raw::write_str(b"prof.dump\0", with_nul) {
        Ok(()) => println!("DUMPED {path}"),
        Err(e) => println!("NO_JEMALLOC prof.dump failed: {e}"),
    }

    std::mem::drop(held);
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn main() {
    // Compiled everywhere so the fixture is never silently missing; see the
    // dependency comment in testbins/Cargo.toml for why jemalloc is scoped to
    // linux-gnu.
    println!("NO_JEMALLOC this fixture links jemalloc only on linux-gnu");
}
