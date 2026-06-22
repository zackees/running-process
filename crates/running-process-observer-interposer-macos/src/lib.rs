//! macOS `DYLD_INSERT_LIBRARIES` interposer for the running-process
//! file-hook tier (#551 slice 5).
//!
//! Built as a cdylib `librunning_process_observer_interposer_macos.dylib`.
//! At load time (when a target process is launched with
//! `DYLD_INSERT_LIBRARIES=…/librunning_process_observer_interposer_macos.dylib`),
//! the dynamic linker loads this library before the C runtime and any
//! symbols we export shadow libc's. Each shadow resolves the real
//! function via `dlsym(RTLD_NEXT, "…")`, invokes it, then emits an
//! `RPO_HOOK …` line on stderr.
//!
//! ## Differences from the Linux interposer
//!
//! The Linux interposer (#551 slice 4) and this one share most of the
//! shape — dlsym-cached real fns in `OnceLock`s, thread-local
//! reentrancy guard, emit on success. Per-OS differences:
//!
//! - **Loader env var**: `DYLD_INSERT_LIBRARIES` (macOS) vs.
//!   `LD_PRELOAD` (Linux). Same idea, different name.
//! - **SIP / hardened-runtime**: macOS refuses to inject into binaries
//!   signed with the hardened runtime + library validation flag
//!   unless they also have `com.apple.security.cs.allow-dyld-environment-variables`
//!   entitlement. System binaries (most of `/usr/bin/*`, `/bin/*`) fall
//!   in this bucket. Same boundary as the rest of the
//!   LaunchedProcessTree tier — works against processes the user owns,
//!   doesn't work against SIP-protected targets.
//! - **Path resolution from fd**: no `/proc/self/fd/<n>` on macOS;
//!   use `fcntl(fd, F_GETPATH, buf)` instead. Lands in slice 5b
//!   alongside `close`/`write`.
//!
//! ## Slice 5a scope (this commit)
//!
//! Scaffold + `open(2)` shadow. Same shape as Linux slice 4a so the
//! emitted line format is identical (`RPO_HOOK file-open path=…
//! flags=… fd=…`). Slices 5b/5c/5d follow the Linux slice 4 cadence:
//! `openat` + fcntl path resolver → `close`/`write` + fd table →
//! `unlink`/`rename` family.

#![cfg(target_os = "macos")]

use std::cell::Cell;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::sync::OnceLock;

type OpenFn = unsafe extern "C" fn(*const c_char, c_int) -> c_int;

static REAL_OPEN: OnceLock<OpenFn> = OnceLock::new();

thread_local! {
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

fn real_open() -> OpenFn {
    *REAL_OPEN.get_or_init(|| unsafe {
        let name = c"open";
        let raw = libc::dlsym(libc::RTLD_NEXT, name.as_ptr());
        if raw.is_null() {
            libc::abort();
        }
        std::mem::transmute::<*mut libc::c_void, OpenFn>(raw)
    })
}

fn emit_line(line: &str) {
    // Direct libc::write to stderr; doesn't recurse through our own
    // shadow (which doesn't exist for `write` yet at slice 5a anyway,
    // but match the Linux pattern so slice 5c doesn't have to
    // refactor).
    unsafe {
        libc::write(
            libc::STDERR_FILENO,
            line.as_ptr() as *const libc::c_void,
            line.len(),
        );
    }
}

fn emit_open(path: &str, flags: c_int, fd: c_int) {
    let line = format!("RPO_HOOK file-open path={path:?} flags={flags} fd={fd}\n");
    emit_line(&line);
}

/// DYLD_INSERT_LIBRARIES shadow for `open(2)` on macOS. Resolves the
/// real implementation lazily via `dlsym(RTLD_NEXT, ...)`, calls it,
/// then emits a `file-open` event on stderr.
///
/// # Safety
///
/// libc-ABI extern "C" fn. The dyld loader invokes it with arguments
/// matching the standard POSIX `open(2)` signature; we trust those.
///
/// `open(2)` is variadic when `O_CREAT` is in flags (mode argument).
/// Rust stable doesn't support `c_variadic`. The mode parameter is
/// ignored — same caveat as the Linux slice 4a interposer. Slice 5d
/// will use the macOS `__syscall` direct route to forward mode
/// correctly.
#[no_mangle]
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int) -> c_int {
    let real = real_open();

    if IN_HOOK.with(|c| c.get()) {
        return real(path, flags);
    }
    IN_HOOK.with(|c| c.set(true));
    let fd = real(path, flags);

    if fd >= 0 && !path.is_null() {
        if let Ok(p) = CStr::from_ptr(path).to_str() {
            emit_open(p, flags, fd);
        }
    }

    IN_HOOK.with(|c| c.set(false));
    fd
}
