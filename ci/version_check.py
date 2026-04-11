"""Lint check: verify that version strings are consistent across all manifests."""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

SOURCES: list[tuple[str, str]] = [
    ("src/running_process/__init__.py", r'^__version__\s*=\s*"([^"]+)"'),
    ("pyproject.toml", r'^version\s*=\s*"([^"]+)"'),
    ("Cargo.toml", r'^version\s*=\s*"([^"]+)"'),
]


def _extract_version(path: Path, pattern: str) -> str | None:
    text = path.read_text(encoding="utf-8")
    match = re.search(pattern, text, re.MULTILINE)
    return match.group(1) if match else None


def main() -> int:
    versions: dict[str, str | None] = {}
    for relpath, pattern in SOURCES:
        versions[relpath] = _extract_version(ROOT / relpath, pattern)

    missing = [k for k, v in versions.items() if v is None]
    if missing:
        for name in missing:
            print(f"ERROR: could not extract version from {name}")
        return 1

    unique = set(versions.values())
    if len(unique) != 1:
        print("ERROR: version mismatch across manifests:")
        for name, ver in versions.items():
            print(f"  {name}: {ver}")
        return 1

    print(f"OK: all versions consistent ({unique.pop()})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
