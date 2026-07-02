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
| `retour` inline detours / DLL injection (`running-process-observer-interposer-windows`) | needs ABI / `iced-x86` validation | out of scope — follow-up |
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
```

A full **build** (rather than `check`) of the GNU target additionally needs
a MinGW-w64 `gcc` on `PATH` for any C-dependency build scripts; `check`
does not link, so it is the cheap gate used here and in CI. The bridge
crate's unit test `conpty_entry_points_are_bound` asserts the ConPTY entry
points resolve to non-null addresses — a runtime linkability check on any
Windows host (MSVC or GNU).

## What remains MSVC-only

`retour`-based detours / DLL injection and the bundled `libsqlite3-sys`
build are **not** yet reachable under GNU. They are tracked as follow-ups
gated on this spike; until they land, a GNU build is limited to the
core + client surface (no `daemon` feature, no file-hook interposer).
