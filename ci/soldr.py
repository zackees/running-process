"""Cargo / maturin command helpers.

`cargo_command` routes through `soldr cargo …` when the `soldr` binary
is on PATH, and falls back to raw `cargo` otherwise. This keeps the
project's stated toolchain policy (CLAUDE.md "soldr-prefixed build
commands") honest from Python and matches the conditional already used in
`install:248`.

CI runners get soldr from `zackees/setup-soldr@v0` (fleet rule CACHE-025,
zackees/ci.yml#209), so on CI every stage takes the `soldr cargo` branch
and its builds reach setup-soldr's zccache-backed build cache. The raw
`cargo` fallback remains for local checkouts without soldr installed.
"""

from __future__ import annotations

import shutil


def cargo_command(*args: str) -> list[str]:
    if shutil.which("soldr"):
        return ["soldr", "cargo", *args]
    return ["cargo", *args]


def maturin_command(python: str, *args: str) -> list[str]:
    return [python, "-m", "maturin", *args]
