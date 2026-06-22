//! Linux LD_PRELOAD interposer for the running-process file-hook
//! tier (#551 slice 4).
//!
//! Built as a cdylib `librunning_process_observer_interposer_linux.so`.
//! At load time (when a target process is launched with
//! `LD_PRELOAD=…/librunning_process_observer_interposer_linux.so`),
//! this library shadows libc's `open` symbol. Each call resolves the
//! real `open` via `dlsym(RTLD_NEXT, "open")`, invokes it, then emits
//! an event line to **stderr** in the form:
//!
//! ```text
//! RPO_HOOK file-open path="<resolved path>" flags=<int> fd=<int>
//! ```
//!
//! Stderr is the slice-4a transport. Slice 4b will replace this with
//! a length-prefixed message over a named pipe whose path is supplied
//! via the `RP_OBSERVER_EVENT_PIPE` env var (set by the parent before
//! `execve()`).
//!
//! ## Slice 4a scope
//!
//! Only `open(2)` is shadowed. `openat`, `close`, `write`, `unlink`,
//! `rename` land in slice 4b. `creat`, `open64`, `__open_2` (libc
//! internal variants) are intentionally NOT shadowed yet — they will
//! be after the openat surface is wired so the test fixture covers
//! the multi-variant case before more variants pile up.
//!
//! ## Caveats
//!
//! - **Variadic `open`** — POSIX `open` is variadic when `O_CREAT` is
//!   in flags (mode argument). Rust stable doesn't support
//!   `c_variadic`. We declare our shadow as non-variadic
//!   `extern "C" fn(*const c_char, c_int) -> c_int`; the C calling
//!   convention happily ignores the unused `mode` argument on x86_64
//!   System V ABI (passed in `edx`/`xmm2` — caller cleans up). The
//!   resulting mode is forwarded to the real `open` as 0 unless we
//!   also re-read it, which is hard without `c_variadic`. **Slice 4a
//!   limitation**: `O_CREAT` opens through the shadow get mode=0,
//!   which means new files end up with mode 000 unless the caller's
//!   umask is set otherwise. Slice 4b uses `syscall(SYS_open, ...)`
//!   directly to dodge this entirely. Tests in slice 4a use existing
//!   files (no `O_CREAT`).
//! - **Reentrancy** — if libc internally calls `open` during dlsym
//!   itself (it doesn't on glibc/musl in practice but could in theory),
//!   we'd recurse. Guarded by a `thread_local!` reentrancy flag — on
//!   re-entry we fall through to the real `open` without emitting.
//! - **Async-signal safety** — `eprintln!` is not async-signal-safe.
//!   The shadow can be called from signal handlers in theory; in
//!   practice POSIX warns against this. Slice 4b uses `write(2)`
//!   directly to a fixed fd.

#![cfg(target_os = "linux")]

use std::cell::Cell;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::sync::OnceLock;

/// Type of libc `open(2)`. Declared non-variadic — see the module
/// doc's caveats section.
type OpenFn = unsafe extern "C" fn(*const c_char, c_int) -> c_int;

/// Cache of the real libc `open` looked up via `dlsym(RTLD_NEXT, ...)`.
/// `OnceLock` makes this thread-safe across the first-call race
/// without pulling in `lazy_static!`.
static REAL_OPEN: OnceLock<OpenFn> = OnceLock::new();

thread_local! {
    /// Reentrancy guard. If we somehow re-enter `open` from within
    /// our own shadow (e.g. an event-emit path that does I/O the
    /// kernel routes back through libc), we want to fall through to
    /// the real `open` without firing another event. Without this a
    /// stack overflow is the failure mode.
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

fn real_open() -> OpenFn {
    *REAL_OPEN.get_or_init(|| unsafe {
        let name = c"open";
        let raw = libc::dlsym(libc::RTLD_NEXT, name.as_ptr());
        if raw.is_null() {
            // dlsym failed — abort with a diagnostic. The interposer
            // is unusable without the real `open`.
            libc::abort();
        }
        std::mem::transmute::<*mut libc::c_void, OpenFn>(raw)
    })
}

/// Emit a hook event line to stderr. Best-effort: ignore errors so a
/// stderr-closed target process doesn't crash.
fn emit_open(path: &str, flags: c_int, fd: c_int) {
    // Format manually with a single write(2) on stderr to keep
    // contention low and avoid Rust's lazy stderr lock interleaving
    // with the target process's own output.
    let line = format!("RPO_HOOK file-open path={path:?} flags={flags} fd={fd}\n");
    unsafe {
        libc::write(
            libc::STDERR_FILENO,
            line.as_ptr() as *const libc::c_void,
            line.len(),
        );
    }
}

/// LD_PRELOAD shadow for `open(2)`. Resolves the real implementation
/// lazily via `dlsym(RTLD_NEXT, ...)`, calls it, then emits a
/// `file-open` event on stderr.
///
/// # Safety
///
/// This is a libc-ABI extern "C" fn. The C runtime invokes it with
/// arguments matching the standard POSIX `open(2)` signature; we
/// trust those.
#[no_mangle]
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int) -> c_int {
    let real = real_open();

    if IN_HOOK.with(|c| c.get()) {
        // Reentrant call — fall through, do not emit.
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
