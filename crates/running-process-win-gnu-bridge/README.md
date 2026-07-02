# running-process-win-gnu-bridge

Build seam (#580) that exposes the **MSVC-obligatory** Windows API surface
to `x86_64-pc-windows-gnu` builds, without regressing the MSVC path.

`publish = false`. On `*-pc-windows-msvc` and on non-Windows hosts this
crate is an inert no-op; on `*-pc-windows-gnu` it forces the linker to bind
the ConPTY import symbols, proving the surface is reachable under the GNU
toolchain with no Windows SDK and no MSVC `link.exe`.

## Mechanism per API surface

| Surface | Mechanism | Notes |
| --- | --- | --- |
| ConPTY — `CreatePseudoConsole` / `ResizePseudoConsole` / `ClosePseudoConsole` | **direct** | `windows-sys` bundles a per-target import library (`windows-targets` → `windows_x86_64_gnu`); the GNU linker resolves these directly. See [`src/conpty.rs`](src/conpty.rs). |
| `retour` inline detours / DLL injection | **out-of-scope** | ABI / `iced-x86` risk. Follow-up. |
| `libsqlite3-sys` (bundled) | **out-of-scope** | Needs a C compiler under GNU (`gcc.exe`). Follow-up. |
| `procdump` / DbgHelp minidump | **out-of-scope** | Dev-only (`test-watchdog`), not on the shipped path. |

No **bridge** (`dlltool`/`.def` import lib, or a `cc`-compiled C shim) is
required for the in-scope ConPTY surface — `windows-sys` links it directly
under GNU. The bridge mechanism is documented as the fallback for any
future in-scope symbol that fails to link directly.

## Building for the GNU target

```bash
soldr rustup target add x86_64-pc-windows-gnu

# This crate (the linkability proof point):
soldr cargo check -p running-process-win-gnu-bridge --target x86_64-pc-windows-gnu

# The ConPTY consumer path (client feature, no daemon):
soldr cargo check -p running-process --no-default-features --features client \
    --target x86_64-pc-windows-gnu
```

See [`docs/win-gnu-bridge.md`](../../docs/win-gnu-bridge.md) for the full
GNU-Windows build notes and what remains MSVC-only.
