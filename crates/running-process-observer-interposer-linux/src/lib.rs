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
//! ## Slice 4a/4b scope
//!
//! Slice 4a: `open(2)`. Slice 4b (this commit): `openat(2)`. Both
//! emit the same `RPO_HOOK file-open` line shape on stderr — `openat`
//! resolves the dirfd to a path via `/proc/self/fd/<dirfd>` when the
//! caller passes a relative `pathname` and `dirfd != AT_FDCWD`, then
//! joins; absolute paths pass through unchanged.
//!
//! `close`, `write`, `unlink`, `rename` land in slice 4c. They each
//! need fd→path tracking (for `close`/`write`) or the same dirfd-join
//! pattern (for `unlinkat`/`renameat`). `creat`, `open64`, `__open_2`
//! (libc internal variants) are intentionally NOT shadowed yet — they
//! follow once we have a test fixture that exercises one variant.
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

/// Type of libc `openat(2)`. Non-variadic for the same reason as
/// [`OpenFn`].
type OpenatFn = unsafe extern "C" fn(c_int, *const c_char, c_int) -> c_int;

/// Cache of the real libc `open` looked up via `dlsym(RTLD_NEXT, ...)`.
/// `OnceLock` makes this thread-safe across the first-call race
/// without pulling in `lazy_static!`.
static REAL_OPEN: OnceLock<OpenFn> = OnceLock::new();

/// Cache of the real libc `openat`.
static REAL_OPENAT: OnceLock<OpenatFn> = OnceLock::new();

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

fn real_openat() -> OpenatFn {
    *REAL_OPENAT.get_or_init(|| unsafe {
        let name = c"openat";
        let raw = libc::dlsym(libc::RTLD_NEXT, name.as_ptr());
        if raw.is_null() {
            libc::abort();
        }
        std::mem::transmute::<*mut libc::c_void, OpenatFn>(raw)
    })
}

/// Resolve a `(dirfd, pathname)` pair to a single best-effort POSIX
/// path. Cases handled:
///
/// - `pathname` is absolute → return it as-is.
/// - `dirfd == AT_FDCWD` (-100) → return `pathname` unchanged; the
///   target process's cwd is the kernel's resolution context anyway.
/// - `dirfd >= 0` and `pathname` is relative → readlink
///   `/proc/self/fd/<dirfd>` to get the absolute dir, join.
///
/// Returns the resolved path as a Rust `String` on success, `None`
/// when the readlink fails (we don't want to block the open just to
/// resolve a name; the syscall has already happened by the time we
/// emit). All filesystem I/O here happens via libc syscalls that are
/// guarded by [`IN_HOOK`] so we don't re-enter our own shadows.
fn resolve_at(dirfd: c_int, pathname: &CStr) -> Option<String> {
    let path_str = pathname.to_str().ok()?;
    if path_str.starts_with('/') {
        return Some(path_str.to_string());
    }
    // AT_FDCWD = -100 per `<fcntl.h>`. libc has the constant but the
    // exact value is part of the stable kernel ABI.
    const AT_FDCWD: c_int = -100;
    if dirfd == AT_FDCWD {
        return Some(path_str.to_string());
    }
    if dirfd < 0 {
        return None;
    }
    // Read /proc/self/fd/<dirfd> via direct readlink to avoid Rust's
    // std::fs which would loop through our own shadows.
    let link = format!("/proc/self/fd/{dirfd}\0");
    let mut buf = [0u8; libc::PATH_MAX as usize];
    let n = unsafe {
        libc::readlink(
            link.as_ptr() as *const c_char,
            buf.as_mut_ptr() as *mut c_char,
            buf.len(),
        )
    };
    if n <= 0 {
        return None;
    }
    let dir = std::str::from_utf8(&buf[..n as usize]).ok()?;
    Some(format!("{dir}/{path_str}"))
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

/// LD_PRELOAD shadow for `openat(2)`. Same flow as [`open`] — resolve
/// the real implementation, call it, then emit a `file-open` event —
/// but joins `dirfd` + `pathname` into a single resolved path via
/// [`resolve_at`] so the consumer sees absolute paths regardless of
/// how the caller invoked `openat`.
///
/// # Safety
///
/// libc-ABI extern "C" fn. The C runtime invokes it with arguments
/// matching POSIX `openat(2)`; we trust those.
#[no_mangle]
pub unsafe extern "C" fn openat(dirfd: c_int, path: *const c_char, flags: c_int) -> c_int {
    let real = real_openat();

    if IN_HOOK.with(|c| c.get()) {
        return real(dirfd, path, flags);
    }
    IN_HOOK.with(|c| c.set(true));
    let fd = real(dirfd, path, flags);

    if fd >= 0 && !path.is_null() {
        if let Some(resolved) = resolve_at(dirfd, CStr::from_ptr(path)) {
            emit_open(&resolved, flags, fd);
        }
    }

    IN_HOOK.with(|c| c.set(false));
    fd
}
