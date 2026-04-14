from __future__ import annotations

import os
import shutil
from functools import lru_cache

DISABLE_SOLDR_ENV = "RUNNING_PROCESS_DISABLE_SOLDR"
FORCE_SOLDR_ENV = "RUNNING_PROCESS_FORCE_SOLDR"
UNSUPPORTED_CARGO_SUBCOMMANDS = {"clippy", "fmt", "llvm-cov"}


def _truthy_env(name: str) -> bool:
    return os.environ.get(name, "").lower() in {"1", "true", "yes", "on"}


@lru_cache(maxsize=1)
def soldr_prefix() -> tuple[str, ...] | None:
    forced = os.environ.get(FORCE_SOLDR_ENV)
    if forced:
        return (forced,)
    if _truthy_env(DISABLE_SOLDR_ENV):
        return None
    if shutil.which("uvx"):
        return ("uvx", "soldr")
    return None


def cargo_command(*args: str) -> list[str]:
    if args and args[0] in UNSUPPORTED_CARGO_SUBCOMMANDS:
        return ["cargo", *args]
    prefix = soldr_prefix()
    if prefix:
        return [*prefix, "cargo", *args]
    return ["cargo", *args]


def maturin_command(python: str, *args: str) -> list[str]:
    return [python, "-m", "maturin", *args]
