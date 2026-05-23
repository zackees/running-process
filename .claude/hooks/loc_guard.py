from __future__ import annotations

import json
import sys
from pathlib import Path

WARN_THRESHOLD = 1000
BLOCK_THRESHOLD = 1500

IGNORED_DIR_PARTS = {
    ".venv",
    "venv",
    "node_modules",
    "target",
    "dist",
    "build",
    ".git",
    "__pycache__",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".next",
    ".nuxt",
    ".cache",
    "vendor",
    ".build",
}

IGNORED_SUFFIXES = (
    ".png",
    ".jpg",
    ".jpeg",
    ".gif",
    ".bmp",
    ".ico",
    ".webp",
    ".svg",
    ".pdf",
    ".zip",
    ".tar",
    ".gz",
    ".bz2",
    ".xz",
    ".7z",
    ".rar",
    ".exe",
    ".dll",
    ".so",
    ".dylib",
    ".o",
    ".obj",
    ".class",
    ".jar",
    ".bin",
    ".dat",
    ".pyc",
    ".whl",
    ".min.js",
    ".min.css",
    ".map",
    ".woff",
    ".woff2",
    ".ttf",
    ".otf",
    ".eot",
    ".mp3",
    ".mp4",
    ".wav",
    ".ogg",
    ".webm",
    ".avi",
    ".mov",
)

IGNORED_FILENAMES = {
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "poetry.lock",
    "Pipfile.lock",
    "uv.lock",
    "Cargo.lock",
    "Gemfile.lock",
    "composer.lock",
    "go.sum",
}


def _extract_file_path(payload: dict) -> str | None:
    tool_input = payload.get("tool_input") or {}
    for key in ("file_path", "notebook_path"):
        value = tool_input.get(key)
        if isinstance(value, str) and value:
            return value
    return None


def _should_skip(path: Path) -> bool:
    if path.name in IGNORED_FILENAMES:
        return True
    lower = path.name.lower()
    if lower.endswith(IGNORED_SUFFIXES):
        return True
    parts_lower = {p.lower() for p in path.parts}
    if parts_lower & IGNORED_DIR_PARTS:
        return True
    return False


def _count_lines(path: Path) -> int | None:
    """Stream-count lines in a file. Returns None if it looks binary."""
    try:
        with path.open("rb") as fh:
            count = 0
            last_byte = b""
            while True:
                chunk = fh.read(65536)
                if not chunk:
                    break
                if b"\x00" in chunk:
                    return None
                count += chunk.count(b"\n")
                last_byte = chunk[-1:]
            if last_byte and last_byte != b"\n":
                count += 1
            return count
    except OSError:
        return None


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except json.JSONDecodeError:
        return 0

    file_path_str = _extract_file_path(payload)
    if not file_path_str:
        return 0

    path = Path(file_path_str)
    try:
        if not path.is_absolute():
            path = path.resolve()
    except OSError:
        return 0

    if not path.is_file():
        return 0

    if _should_skip(path):
        return 0

    loc = _count_lines(path)
    if loc is None:
        return 0

    if loc > BLOCK_THRESHOLD:
        sys.stderr.write(
            f"FILE TOO LARGE: {file_path_str} is {loc} LOC "
            f"(hard limit {BLOCK_THRESHOLD}). Refactor immediately - "
            f"split into smaller modules before continuing.\n"
        )
        return 2

    if loc > WARN_THRESHOLD:
        sys.stderr.write(
            f"WARNING: {file_path_str} is {loc} LOC "
            f"(soft limit {WARN_THRESHOLD}, hard limit {BLOCK_THRESHOLD}). "
            f"Plan to refactor soon.\n"
        )
        message = {
            "systemMessage": (
                f"{file_path_str} is {loc} LOC (over the {WARN_THRESHOLD} soft limit). "
                f"Consider refactoring."
            )
        }
        json.dump(message, sys.stdout)
        sys.stdout.write("\n")
        return 0

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
