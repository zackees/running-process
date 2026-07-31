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

/// Read jemalloc's `opt.prof`: was profiling enabled at process start?
///
/// `Err` carries jemalloc's errno. `ENOENT` there means the allocator was
/// built without profiling support at all, which is a different problem from
/// profiling being off — hence the separate outcomes reported by `main`.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn profiling_enabled() -> Result<bool, i32> {
    let mut value: bool = false;
    let mut len = std::mem::size_of::<bool>();
    // SAFETY: `opt.prof` is a bool-typed mallctl. `value` and `len` are a
    // matching pair, and the name is NUL-terminated as mallctl requires.
    let rc = unsafe {
        tikv_jemalloc_sys::mallctl(
            c"opt.prof".as_ptr(),
            std::ptr::addr_of_mut!(value).cast(),
            std::ptr::addr_of_mut!(len),
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 {
        Ok(value)
    } else {
        Err(rc)
    }
}

/// Ask jemalloc to write a heap dump to `path`.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn dump_to(path: &str) -> Result<(), i32> {
    let c_path = std::ffi::CString::new(path).expect("dump path has no interior NUL");
    let mut arg = c_path.as_ptr();
    // SAFETY: `prof.dump` is write-only and takes a `*const c_char` by value,
    // so the argument is a pointer to that pointer with a matching length.
    // jemalloc copies the path during the call, and `c_path` outlives it.
    let rc = unsafe {
        tikv_jemalloc_sys::mallctl(
            c"prof.dump".as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(arg).cast(),
            std::mem::size_of::<*const std::os::raw::c_char>(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(rc)
    }
}

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

    match profiling_enabled() {
        Ok(true) => {}
        Ok(false) => {
            println!("PROFILING_OFF");
            return;
        }
        Err(code) => {
            println!("NO_JEMALLOC mallctl(opt.prof) failed with errno {code}");
            return;
        }
    }

    // Held across the dump: jemalloc reports what is live, so releasing this
    // first would produce a dump that correctly shows nothing leaking.
    let held = leak_here(BLOCKS);

    match dump_to(&path) {
        Ok(()) => println!("DUMPED {path}"),
        Err(code) => println!("NO_JEMALLOC mallctl(prof.dump) failed with errno {code}"),
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
