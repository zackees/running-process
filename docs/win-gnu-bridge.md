# GNU-Windows bridge (#580)

Windows builds of this workspace are effectively pinned to
`x86_64-pc-windows-msvc` (see the "Windows Native Build Rules" section of
[`CLAUDE.md`](../CLAUDE.md)). The `running-process-win-gnu-bridge` crate is
the build seam that lets `x86_64-pc-windows-gnu` builds reach the same
**MSVC-obligatory** Windows API surface the rest of the workspace depends
on, without changing the MSVC path.

The guiding principle is **direct where possible, bridge only where
necessary**:

1. **Direct** — where the GNU toolchain can already link the symbol, use it
   as-is. This is the case for the whole ConPTY surface: `windows-sys`
   bundles a per-target import library (`windows-targets` →
   `windows_x86_64_gnu`), so the GNU linker resolves
   `CreatePseudoConsole` / `ResizePseudoConsole` / `ClosePseudoConsole`
   with no Windows SDK and no MSVC `link.exe`.
2. **Bridge** — where a symbol cannot link directly, provide a thin bridge:
   a generated import library (`dlltool` + a `.def` file) or a small C shim
   compiled with the GNU/clang C compiler, exposing a stable Rust-facing
   FFI. No in-scope symbol needs this today; it is the documented fallback.

## Mechanism per API surface

| Surface | Mechanism | Status |
| --- | --- | --- |
| ConPTY (`CreatePseudoConsole` / `ResizePseudoConsole` / `ClosePseudoConsole`) | **direct** (`windows-sys` bundled `-gnu` import lib) | in scope, done |
| `retour` inline detours / DLL injection (`running-process-observer-interposer-windows`) | **direct** (`retour` + `iced-x86` + `windows-sys` all link under `-gnu`) | in scope, done |
| `libsqlite3-sys` bundled (daemon feature) | needs a C compiler (`gcc.exe`) under GNU | out of scope — follow-up |
| `procdump` / DbgHelp minidump (`test-watchdog`) | dev-only, not on the shipped path | out of scope |

## Building and checking the GNU target

```bash
# One-time: install the GNU std for the target.
soldr rustup target add x86_64-pc-windows-gnu

# The bridge crate — the linkability proof point.
soldr cargo check -p running-process-win-gnu-bridge --target x86_64-pc-windows-gnu

# The ConPTY consumer path (client feature, no daemon so no libsqlite3-sys).
soldr cargo check -p running-process --no-default-features --features client \
    --target x86_64-pc-windows-gnu

# The file-hook observer interposer path.
soldr cargo build -p running-process-observer-interposer-windows \
    --target x86_64-pc-windows-gnu
soldr cargo build -p running-process-observer --features embed-helper \
    --target x86_64-pc-windows-gnu
soldr cargo test -p running-process-observer --features embed-helper \
    --test interposer_integration_windows \
    --target x86_64-pc-windows-gnu
```

A full **build** (rather than `check`) of the GNU target additionally needs
a MinGW-w64 `gcc` on `PATH` for any C-dependency build scripts; `check`
does not link, so it is the cheap gate used here and in CI. The bridge
crate's unit test `conpty_entry_points_are_bound` asserts the ConPTY entry
points resolve to non-null addresses — a runtime linkability check on any
Windows host (MSVC or GNU).

The Windows interposer smoke test builds the DLL and
`testbin-createfilew-probe` for the same active target triple as the test
binary. Under `x86_64-pc-windows-gnu`, it injects the GNU-built DLL via
`CreateRemoteThread(LoadLibraryW)` and waits for a real `RPO_HOOK file-open`
line emitted by the `retour::RawDetour` `CreateFileW` hook. That validates
the GNU ABI path for the inline trampoline, the `iced-x86` prologue decode,
the `VirtualProtect`-backed patch install, and the deferred `DllMain` worker
thread pattern.

## What remains MSVC-only

The bundled `libsqlite3-sys` build used by the `daemon` feature is the only
remaining GNU follow-up from this bridge spike. It needs a MinGW-w64
`gcc.exe` on `PATH` so the sqlite amalgamation can compile and link under
`x86_64-pc-windows-gnu`. Until that lands, a GNU build is limited to the
core, client, bridge, and file-hook interposer surfaces (no `daemon`
feature).
