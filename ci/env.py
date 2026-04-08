from __future__ import annotations

import os
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parent.parent
PROJECT_CARGO_HOME = PROJECT_ROOT / ".cargo"
PROJECT_RUSTUP_HOME = PROJECT_ROOT / ".rustup"


def find_cargo_bin() -> str | None:
    for candidate in (
        os.environ.get("CARGO_HOME"),
        str(PROJECT_CARGO_HOME),
        str(Path.home() / ".cargo"),
        os.path.join(os.environ.get("USERPROFILE", ""), ".cargo"),
    ):
        if not candidate:
            continue
        bin_dir = Path(candidate) / "bin"
        if bin_dir.is_dir():
            return str(bin_dir)
    return None


def activate() -> None:
    os.environ["CARGO_HOME"] = str(PROJECT_CARGO_HOME)
    os.environ["RUSTUP_HOME"] = str(PROJECT_RUSTUP_HOME)
    cargo_bin = find_cargo_bin()
    if not cargo_bin:
        return

    current_path = os.environ.get("PATH", "")
    path_parts = current_path.split(os.pathsep) if current_path else []
    normalized_cargo_bin = os.path.normcase(os.path.normpath(cargo_bin))
    normalized_parts = {
        os.path.normcase(os.path.normpath(part))
        for part in path_parts
        if part
    }
    if normalized_cargo_bin in normalized_parts:
        return
    os.environ["PATH"] = (
        cargo_bin if not current_path else cargo_bin + os.pathsep + current_path
    )


def clean_env() -> dict[str, str]:
    activate()
    env = os.environ.copy()
    env.pop("VIRTUAL_ENV", None)
    env.setdefault("PYTHONUTF8", "1")
    return env
