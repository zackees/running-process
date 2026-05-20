"""Cargo / maturin command helpers.

Historical note: this module wrapped cargo with `soldr cargo …` so CI
runners would route through soldr's managed toolchain + zccache.
After zccache caused macOS-only build-script failures (PR #116), CI was
switched to the standard `dtolnay/rust-toolchain` + `Swatinem/rust-cache`
combo, and `cargo_command` was reduced to a passthrough. Local developers
who want soldr can still invoke `./_cargo` from the repo root.
"""

from __future__ import annotations


def cargo_command(*args: str) -> list[str]:
    return ["cargo", *args]


def maturin_command(python: str, *args: str) -> list[str]:
    return [python, "-m", "maturin", *args]
