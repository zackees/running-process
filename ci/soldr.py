"""Soldr command helpers for Rust and Python build orchestration."""

from __future__ import annotations

import shutil

# Coverage builds share the target tree used by cargo-llvm-cov. This marker
# controls target selection only; it never changes the Cargo command route.
COVERAGE_ENV = "RUNNING_PROCESS_COVERAGE"


def soldr_command(*args: str) -> list[str]:
    prefix = ["soldr"] if shutil.which("soldr") else ["uvx", "soldr"]
    return [*prefix, *args]


def cargo_command(*args: str) -> list[str]:
    return soldr_command("cargo", *args)


def maturin_command(*args: str) -> list[str]:
    return soldr_command("maturin", *args)
