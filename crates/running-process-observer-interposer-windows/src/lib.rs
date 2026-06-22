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
//! 4. `DllMain` installs `retour-rs`-backed detours on the Win32
//!    file APIs (`CreateFileW`, `WriteFile`, `CloseHandle`,
//!    `DeleteFileW`, `MoveFileExW`). Each detour calls the original
//!    after emitting an `RPO_HOOK …` line on stderr matching the
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
//! ## Slice 6a scope (this commit)
//!
//! Scaffold + inert `DllMain`. The DLL compiles to a cdylib that
//! can be `LoadLibrary`'d, but installs no detours yet — DllMain
//! just returns TRUE. Confirms the workspace + cdylib build chain
//! for Windows mirrors what we have for Linux/macOS.
//!
//! Slice 6b adds:
//! - `retour` dependency.
//! - `static_detour!` declarations for each Win32 file API.
//! - DllMain installs the detours under `DLL_PROCESS_ATTACH`,
//!   uninstalls under `DLL_PROCESS_DETACH`.
//! - Re-injection into `CreateProcess`-spawned children
//!   (`CreateProcessW` hook that injects this DLL into the new
//!   process before it runs `main`, matching the env-var
//!   propagation that LD_PRELOAD / DYLD_INSERT_LIBRARIES get for
//!   free on Unix).
//!
//! Slice 6c adds the sidecar-side injection vehicle that drives
//! `CreateRemoteThread(LoadLibraryW, dll_path)` into freshly
//! spawned children of the running-process daemon.

#![cfg(target_os = "windows")]

use winapi::shared::minwindef::{BOOL, DWORD, HINSTANCE, LPVOID, TRUE};
use winapi::um::winnt::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH};

/// DLL entry point. Windows calls this when the DLL is loaded into a
/// process (via `LoadLibrary` from the sidecar injector) and when
/// it's unloaded.
///
/// Slice 6a: inert — returns `TRUE` without installing any detours.
/// Slice 6b will install the `retour-rs` detours under
/// `DLL_PROCESS_ATTACH`.
///
/// # Safety
///
/// Called by the Windows loader with the documented `DllMain`
/// signature. No safety contract beyond "obey the
/// `DllMain` restrictions" — no synchronization primitives in
/// `DLL_PROCESS_ATTACH`, no waiting on threads, no Win32 calls that
/// would re-enter the loader lock. Slice 6b detours installation
/// happens to be loader-lock-safe (retour-rs writes raw bytes via
/// `WriteProcessMemory` on `GetCurrentProcess()` without acquiring
/// any loader resources).
#[no_mangle]
pub unsafe extern "system" fn DllMain(
    _hinst: HINSTANCE,
    reason: DWORD,
    _reserved: LPVOID,
) -> BOOL {
    match reason {
        DLL_PROCESS_ATTACH => {
            // Slice 6b: install detours here.
        }
        DLL_PROCESS_DETACH => {
            // Slice 6b: uninstall detours here.
        }
        _ => {
            // DLL_THREAD_ATTACH / DLL_THREAD_DETACH — no-op.
        }
    }
    TRUE
}
