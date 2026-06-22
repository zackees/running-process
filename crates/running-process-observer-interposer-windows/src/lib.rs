//! Windows DLL-injection interposer for the running-process
//! file-hook tier (#551 slice 6).
//!
//! Built as a cdylib `running_process_observer_interposer_windows.dll`.
//! Unlike the Linux LD_PRELOAD and macOS DYLD_INSERT_LIBRARIES
//! interposers — which the dynamic linker loads automatically when
//! the appropriate env var is set — Windows has no equivalent loader
//! env var. Injection happens via:
//!
//! 1. The parent (running-process daemon) allocates memory in the
//!    target process via `VirtualAllocEx` and writes the path to
//!    this DLL into it.
//! 2. The parent calls
//!    `CreateRemoteThread(target, LoadLibraryW, dll_path_ptr)`,
//!    which forks a thread in the target that calls
//!    `LoadLibraryW(dll_path)`.
//! 3. `LoadLibraryW` brings this DLL into the target's address space
//!    and calls our `DllMain` with `DLL_PROCESS_ATTACH`.
//! 4. `DllMain` installs `retour`-backed inline detours on the Win32
//!    file APIs. Each detour calls the original via a trampoline,
//!    emitting an `RPO_HOOK …` line on stderr matching the
//!    Linux + macOS interposer format.
//!
//! ## AV / EDR exposure
//!
//! The injection vehicle (`CreateRemoteThread` + `LoadLibraryW`) is
//! the prototypical "process injection" pattern AV/EDR products flag
//! aggressively. The `#551` design body documents the mitigation:
//! injection lives in the **sidecar helper binary**
//! (`running-process-observer-helper`, slices 1–2) which is embedded
//! in the `running-process-observer` crate via `include_bytes!` and
//! extracted to a per-user cache at first use. The main
//! `running-process` crate stays free of injection symbols entirely.
//! This DLL (the **payload**) doesn't itself call the injection
//! primitives; only the sidecar does.
//!
//! ## Slice 6b scope (this commit)
//!
//! Wire up `retour::RawDetour` + `windows-sys` and install the
//! **first** inline detour — `CreateFileW`. This proves the
//! detour-install path works end-to-end on the stable toolchain
//! (resolve target via `GetModuleHandle` + `GetProcAddress`,
//! register hook via `RawDetour::new`, enable on
//! `DLL_PROCESS_ATTACH`, call original through trampoline). The
//! other four file APIs (`WriteFile`, `CloseHandle`, `DeleteFileW`,
//! `MoveFileExW`) land in slice 6c with the same shape — keeping
//! the surface area small per PR lets us validate one detour at a
//! time.
//!
//! Slice 6c adds `WriteFile`, `CloseHandle`, `DeleteFileW`,
//! `MoveFileExW` (matching the macOS slice 5b bundle), plus a
//! HANDLE→path table so close/write events can resolve the path
//! the way the macOS interposer does via `fcntl(F_GETPATH)` and
//! the Linux one via `/proc/self/fd/<n>`.
//!
//! Slice 6d adds the sidecar-side injection vehicle that drives
//! `CreateRemoteThread(LoadLibraryW, dll_path)` into freshly
//! spawned children of the running-process daemon.

#![cfg(target_os = "windows")]

use std::cell::Cell;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::sync::atomic::{AtomicPtr, Ordering};

use retour::RawDetour;
use windows_sys::Win32::Foundation::{BOOL, HANDLE, INVALID_HANDLE_VALUE, TRUE};
use windows_sys::Win32::Storage::FileSystem::WriteFile;
use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::SystemServices::{
    DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH,
};

// ── Function-pointer types ──

type CreateFileWFn = unsafe extern "system" fn(
    *const u16,                // lpFileName (LPCWSTR)
    u32,                       // dwDesiredAccess
    u32,                       // dwShareMode
    *const core::ffi::c_void,  // lpSecurityAttributes
    u32,                       // dwCreationDisposition
    u32,                       // dwFlagsAndAttributes
    HANDLE,                    // hTemplateFile
) -> HANDLE;

// ── Trampolines ──

/// Pointer to the trampoline that calls the original `CreateFileW`.
/// Populated when the detour is enabled in `install_detours()`.
///
/// We can't store a `CreateFileWFn` directly in an `AtomicPtr<F>`
/// because fn-pointers and data-pointers have different niches in
/// Rust's typesystem. Instead store the raw bytes and transmute
/// at the call site.
static REAL_CREATE_FILE_W: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

thread_local! {
    /// Reentrancy guard. We never want our hook body to re-enter
    /// itself (e.g. if `format!` inside emit allocates and the
    /// allocator opens a heap-backed file). Same pattern as the
    /// Linux + macOS interposers.
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

// ── Event emission ──

/// Write `bytes` to stderr via `WriteFile`. For slice 6b only
/// `CreateFileW` is detoured so calling the real `WriteFile`
/// directly is fine. Slice 6c switches this to the original
/// `WriteFile` trampoline once that detour exists, so we don't
/// observe our own emissions.
fn emit_bytes(bytes: &[u8]) {
    unsafe {
        let h = GetStdHandle(STD_ERROR_HANDLE);
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return;
        }
        let mut written: u32 = 0;
        let _ = WriteFile(
            h,
            bytes.as_ptr(),
            bytes.len() as u32,
            &mut written,
            core::ptr::null_mut(),
        );
    }
}

fn emit_line(line: &str) {
    emit_bytes(line.as_bytes());
}

fn emit_open(path: &str, access: u32, disposition: u32, handle: HANDLE) {
    // `flags` slot reuses the Linux/macOS field name; for Windows we
    // pack `dwDesiredAccess` and `dwCreationDisposition` into a
    // structured value the downstream parser can split on.
    emit_line(&format!(
        "RPO_HOOK file-open path={path:?} access=0x{access:08x} disposition={disposition} handle={handle:p}\n",
    ));
}

// ── Path helpers ──

/// Read a NUL-terminated UTF-16 string into a Rust `String`. Returns
/// `None` for a null pointer or invalid UTF-16. Bounded at 32k
/// `wchar_t`s to avoid runaway scans of unterminated buffers (the
/// Win32 `MAX_PATH`-extended form caps at ~32760 wide chars).
fn wide_cstr_to_string(ptr: *const u16) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let mut len = 0usize;
    while len < 32_768 {
        let c = unsafe { *ptr.add(len) };
        if c == 0 {
            break;
        }
        len += 1;
    }
    if len == 32_768 {
        return None;
    }
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    OsString::from_wide(slice).into_string().ok()
}

// ── Detour body ──

/// Detour body for `CreateFileW`. Calls the original via the
/// stashed trampoline, emits an `RPO_HOOK file-open` line on
/// success, and returns the original handle. Failures
/// (`INVALID_HANDLE_VALUE`) pass through without emitting.
unsafe extern "system" fn create_file_w_detour(
    lp_file_name: *const u16,
    dw_desired_access: u32,
    dw_share_mode: u32,
    lp_security_attributes: *const core::ffi::c_void,
    dw_creation_disposition: u32,
    dw_flags_and_attributes: u32,
    h_template_file: HANDLE,
) -> HANDLE {
    let trampoline_ptr = REAL_CREATE_FILE_W.load(Ordering::Acquire);
    // Should never be null after install_detours() runs, but if for
    // any reason it is, fail closed (return INVALID_HANDLE_VALUE).
    // Calling the original via GetProcAddress at this point would
    // mean *not* going through the trampoline, defeating the detour.
    if trampoline_ptr.is_null() {
        return INVALID_HANDLE_VALUE;
    }
    let original: CreateFileWFn = std::mem::transmute(trampoline_ptr);

    let handle = original(
        lp_file_name,
        dw_desired_access,
        dw_share_mode,
        lp_security_attributes,
        dw_creation_disposition,
        dw_flags_and_attributes,
        h_template_file,
    );

    if IN_HOOK.with(|c| c.get()) {
        return handle;
    }
    IN_HOOK.with(|c| c.set(true));
    if handle != INVALID_HANDLE_VALUE {
        if let Some(path) = wide_cstr_to_string(lp_file_name) {
            emit_open(&path, dw_desired_access, dw_creation_disposition, handle);
        }
    }
    IN_HOOK.with(|c| c.set(false));

    handle
}

// ── DllMain ──

/// Install all detours that are wired up for this slice. Called
/// from `DllMain` under `DLL_PROCESS_ATTACH`.
///
/// Errors are deliberately swallowed: a hook-install failure should
/// not prevent the host process from starting. Slice 6d will report
/// install status via a separate IPC channel back to the daemon.
unsafe fn install_detours() {
    // Wide-encode "kernel32.dll\0" for GetModuleHandleW. Encoded
    // inline rather than via a UTF-16 literal to keep
    // `windows-sys` the only Win32 dep.
    let kernel32: [u16; 13] = [
        b'k' as u16, b'e' as u16, b'r' as u16, b'n' as u16, b'e' as u16,
        b'l' as u16, b'3' as u16, b'2' as u16, b'.' as u16, b'd' as u16,
        b'l' as u16, b'l' as u16, 0,
    ];
    let module = GetModuleHandleW(kernel32.as_ptr());
    if module.is_null() {
        return;
    }
    let Some(target) =
        GetProcAddress(module, c"CreateFileW".as_ptr() as *const u8)
    else {
        return;
    };

    let Ok(detour) =
        RawDetour::new(target as *const (), create_file_w_detour as *const ())
    else {
        return;
    };
    if detour.enable().is_err() {
        return;
    }

    // `RawDetour::trampoline()` returns `&()` (a borrow into the
    // detour's owned trampoline allocation). We need a raw pointer
    // we can `mem::transmute` to the original fn signature inside
    // the hook body. The double cast `&() -> *const () -> *mut ()`
    // strips the borrow without mutability checks (the trampoline
    // is immutable code we only ever read-execute).
    REAL_CREATE_FILE_W
        .store(detour.trampoline() as *const () as *mut (), Ordering::Release);

    // Leak the RawDetour so its Drop doesn't disable the hook when
    // this scope exits. The detour lives for the rest of the
    // process; on DLL_PROCESS_DETACH the OS is tearing the address
    // space down anyway, so we don't need to recover the storage.
    core::mem::forget(detour);
}

/// DLL entry point. Windows calls this when the DLL is loaded into a
/// process (via `LoadLibrary` from the sidecar injector) and when
/// it's unloaded.
///
/// # Safety
///
/// Called by the Windows loader with the documented `DllMain`
/// signature. retour-rs writes raw bytes via `VirtualProtect` +
/// memcpy against `GetCurrentProcess()` without acquiring any
/// loader resources, so detour installation is loader-lock-safe.
#[no_mangle]
pub unsafe extern "system" fn DllMain(
    _hinst: *mut core::ffi::c_void,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> BOOL {
    match reason {
        DLL_PROCESS_ATTACH => {
            install_detours();
        }
        DLL_PROCESS_DETACH => {
            // The detour was leaked in install_detours so it stays
            // installed for the life of the process. The OS is
            // tearing the address space down — there's nothing
            // useful to do here.
        }
        _ => {
            // DLL_THREAD_ATTACH / DLL_THREAD_DETACH — no-op.
        }
    }
    TRUE
}
