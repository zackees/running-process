from __future__ import annotations

UNSUPPORTED_CARGO_SUBCOMMANDS = {"clippy", "fmt", "llvm-cov"}


def cargo_command(*args: str) -> list[str]:
    if args and args[0] in UNSUPPORTED_CARGO_SUBCOMMANDS:
        return ["cargo", *args]
    return ["soldr", "cargo", *args]


def maturin_command(python: str, *args: str) -> list[str]:
    return [python, "-m", "maturin", *args]
