"""Manifest-driven ratchet for #850 async platform-boundary migration."""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RAW_MANIFEST = ROOT / "raw_platform_apis.toml"
BASELINE = ROOT / "platform_compliance_baseline.toml"
INTERNAL_ROOT = ROOT / "crates" / "running-process-platform-internal"
PYTHON_ASYNC_ROOT = ROOT / "src" / "running_process" / "asyncio"
FORBIDDEN_PYTHON_ASYNC_PATTERNS = {
    r"asyncio\.to_thread\s*\(": "native async bridge instead of asyncio.to_thread",
    r"run_in_executor\s*\(": "native async bridge instead of an executor wrapper",
    r"threading\.Thread\s*\(": "actor-backed async lifecycle instead of a Python reader thread",
}


@dataclass(frozen=True)
class ExceptionEntry:
    identifier: str
    symbol: str
    path: Path
    max_occurrences: int
    owner: str
    justification: str
    expires_phase: int


def _value(line: str) -> str:
    _, value = line.split("=", 1)
    return value.strip().strip('"')


def _entries() -> list[ExceptionEntry]:
    entries: list[dict[str, str]] = []
    current: dict[str, str] | None = None
    for line in BASELINE.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line == "[[exception]]":
            current = {}
            entries.append(current)
            continue
        if current is not None and "=" in line:
            key, _ = line.split("=", 1)
            current[key.strip()] = _value(line)

    required = {
        "id",
        "symbol",
        "path",
        "max_occurrences",
        "owner",
        "justification",
        "expires_phase",
    }
    failures: list[str] = []
    result: list[ExceptionEntry] = []
    for entry in entries:
        missing = sorted(required - entry.keys())
        if missing:
            failures.append(
                f"baseline exception {entry.get('id', '<unnamed>')} missing {', '.join(missing)}"
            )
            continue
        if not entry["justification"] or "*" in entry["path"]:
            failures.append(
                f"baseline exception {entry['id']} has an invalid path or justification"
            )
            continue
        result.append(
            ExceptionEntry(
                identifier=entry["id"],
                symbol=entry["symbol"],
                path=Path(entry["path"]),
                max_occurrences=int(entry["max_occurrences"]),
                owner=entry["owner"],
                justification=entry["justification"],
                expires_phase=int(entry["expires_phase"]),
            )
        )
    if failures:
        raise ValueError("; ".join(failures))
    return result


def _raw_symbols() -> set[str]:
    manifest = RAW_MANIFEST.read_text(encoding="utf-8")
    return set(re.findall(r'^path\s*=\s*"([^"]+)"$', manifest, re.MULTILINE))


def _token(symbol: str) -> str:
    return symbol.rsplit("::", 1)[-1]


def _occurrences(symbol: str, source: str) -> int:
    # Tokio's Phase-0 debt is intentionally written fully qualified, so count
    # that exact capability rather than unrelated prose mentioning Command.
    if symbol.startswith("tokio::process::"):
        return len(re.findall(re.escape(symbol), source))
    return len(re.findall(rf"\b{re.escape(_token(symbol))}\b", source))


def check() -> list[str]:
    failures: list[str] = []
    try:
        entries = _entries()
    except ValueError as error:
        return [str(error)]

    raw_symbols = _raw_symbols()
    seen: set[tuple[str, Path]] = set()
    for entry in entries:
        key = (entry.symbol, entry.path)
        if key in seen:
            failures.append(f"duplicate baseline exception for {entry.symbol} at {entry.path}")
            continue
        seen.add(key)
        if entry.symbol not in raw_symbols:
            failures.append(
                f"{entry.identifier}: {entry.symbol} is absent from raw_platform_apis.toml"
            )
        source = ROOT / entry.path
        if not source.is_file():
            failures.append(f"{entry.identifier}: missing source file {entry.path}")
            continue
        occurrences = _occurrences(entry.symbol, source.read_text(encoding="utf-8"))
        if occurrences > entry.max_occurrences:
            failures.append(
                f"{entry.identifier}: {entry.path} has {occurrences} uses of "
                f"{_token(entry.symbol)} "
                f"(baseline allows {entry.max_occurrences}); migrate the use or lower the debt"
            )

    for raw_symbol in raw_symbols:
        if raw_symbol.startswith("tokio::process::"):
            for source in ROOT.glob("crates/**/*.rs"):
                if INTERNAL_ROOT in source.parents:
                    continue
                if raw_symbol not in source.read_text(encoding="utf-8"):
                    continue
                rel = source.relative_to(ROOT)
                if (raw_symbol, rel) not in seen:
                    failures.append(
                        f"unbaselined raw platform API {raw_symbol} in {rel}; use a blessed "
                        "capability or add an exact ratchet entry"
                    )

    for source in PYTHON_ASYNC_ROOT.rglob("*.py"):
        text = source.read_text(encoding="utf-8")
        rel = source.relative_to(ROOT)
        for pattern, replacement in FORBIDDEN_PYTHON_ASYNC_PATTERNS.items():
            if re.search(pattern, text):
                failures.append(
                    f"forbidden async Python pattern {pattern} in {rel}; use the {replacement}"
                )
    return failures


def main() -> int:
    failures = check()
    if failures:
        print("async compliance guard failed:")
        for failure in failures:
            print(f"  {failure}")
        return 1
    print("async compliance guard passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
