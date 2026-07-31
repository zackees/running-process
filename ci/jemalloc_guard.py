"""Guard: jemalloc is being removed, so no NEW references may appear.

The project is migrating heap profiling off jemalloc and onto
`mimalloc-pprof`, which emits pprof directly and so removes the need for the
jeprof text parser entirely. Until that migration lands there is a window
where jemalloc is still present and someone could reasonably add more of it
without knowing it is on its way out.

This is a ratchet, not a ban. It pins the sites that exist today with their
occurrence counts and fails if a new file appears or an existing count grows.
Deleting references is always allowed — the counts here are ceilings, not
targets. As the purge proceeds, entries come out of `KNOWN_SITES`; when the
table is empty the guard becomes an absolute prohibition and the tracking
issue can close.

A plain "grep for jemalloc and fail" could not be committed until the very
last reference was gone, which is exactly the period when the guard is most
useful.

Run alone with:
    uv run --no-project python -m ci.jemalloc_guard
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Tokens that mean "jemalloc is involved here". `heap_v2` and `jeprof` are
# jemalloc's dump format; `MALLOC_CONF` / `_RJEM_` are how its profiler is
# switched on; `tikv-jemalloc` is the crate family.
PATTERN = re.compile(
    r"jemalloc|jeprof|MALLOC_CONF|_RJEM_|heap_v2|tikv-jemalloc", re.IGNORECASE
)

# Extensions worth scanning. Deliberately excludes Cargo.lock: it is generated,
# and its jemalloc entries disappear on their own once the manifests stop
# asking for the crates.
SUFFIXES = {".rs", ".toml", ".md", ".py", ".yml", ".yaml", ".json", ".sh"}

# This guard's own module name, which its callers must spell out.
#
# Stripped before counting rather than exempting `ci/lint.py` and
# `tests/test_ci_lint.py` wholesale: those files should still be caught if they
# ever gain a *real* jemalloc reference. Only the name of the machinery that
# removes jemalloc is ignored, not every mention in the files that invoke it.
SELF_REFERENCE = re.compile(r"jemalloc_guard")

# The guard itself is exempt outright — it has to spell out every token it
# forbids in order to search for them.
EXEMPT = {"ci/jemalloc_guard.py"}

# The jemalloc that exists today, path -> occurrence count.
#
# Every entry is a thing the purge has to delete. Shrinking a number is fine
# and needs no edit here (the check is `>`), but removing the last reference in
# a file should also remove its row, so the remaining work stays readable.
KNOWN_SITES: dict[str, int] = {
    # The jeprof text parser and its pprof lowering. `mimalloc-pprof` emits
    # pprof directly, so this module is expected to be deleted outright rather
    # than ported.
    "crates/running-process-probe-daemon/src/profile/heap.rs": 42,
    "crates/running-process-probe-daemon/src/profile/heap/tests.rs": 52,
    # End-to-end coverage driving the jemalloc fixture.
    "crates/running-process-probe-daemon/tests/profile_heap_test.rs": 25,
    # The fixture application and its allocator dependency.
    "testbins/src/bin/jemalloc_leaker.rs": 23,
    "testbins/Cargo.toml": 11,
}


def _relative(path: Path) -> str:
    return path.relative_to(ROOT).as_posix()


def _iter_source_files() -> list[Path]:
    """Git-tracked source files only.

    Deliberately not a filesystem walk. This repo keeps a vendored toolchain
    and crate registry under `.cargo/` and `.rustup/` — both gitignored, both
    full of jemalloc's own source. Walking the tree found ~200 "violations" in
    upstream crates that no purge could ever remove. What this guard governs is
    committed source, and `git ls-files` is exactly that set.
    """
    result = subprocess.run(
        ["git", "ls-files", "-z"],
        cwd=ROOT,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        raise SystemExit(
            "jemalloc-guard: `git ls-files` failed; this guard scans tracked "
            f"files and needs a git checkout.\n{result.stderr.decode(errors='replace')}"
        )

    found: list[Path] = []
    for entry in result.stdout.decode(errors="replace").split("\0"):
        if not entry:
            continue
        path = ROOT / entry
        if path.suffix in SUFFIXES and path.is_file():
            found.append(path)
    return found


def count_occurrences(path: Path) -> int:
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return 0
    return len(PATTERN.findall(SELF_REFERENCE.sub("", text)))


def scan() -> dict[str, int]:
    """Every scanned file that mentions jemalloc, with its occurrence count."""
    counts: dict[str, int] = {}
    for path in _iter_source_files():
        if _relative(path) in EXEMPT:
            continue
        found = count_occurrences(path)
        if found:
            counts[_relative(path)] = found
    return counts


def check() -> list[str]:
    failures: list[str] = []
    actual = scan()

    for rel, found in sorted(actual.items()):
        allowed = KNOWN_SITES.get(rel)
        if allowed is None:
            failures.append(
                f"{rel}: {found} jemalloc reference(s) in a file that had none.\n"
                "    jemalloc is being removed in favour of `mimalloc-pprof`; "
                "do not add more.\n"
                "    If this is part of the migration itself, add the path to "
                "KNOWN_SITES in ci/jemalloc_guard.py with a comment saying why."
            )
        elif found > allowed:
            failures.append(
                f"{rel}: {found} jemalloc reference(s), up from {allowed}.\n"
                "    This file is on the removal list; it should be shrinking, "
                "not growing."
            )

    # A stale row is its own bug: it makes the remaining work look larger than
    # it is, and it is how a ratchet quietly stops ratcheting.
    for rel in sorted(KNOWN_SITES):
        if rel not in actual:
            failures.append(
                f"{rel}: listed in KNOWN_SITES but now has no jemalloc "
                "references.\n"
                "    Delete its row — the list should shrink as the purge "
                "proceeds."
            )
    return failures


def main() -> int:
    failures = check()
    if failures:
        print("jemalloc-guard: FAILED", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1

    remaining = sum(KNOWN_SITES.values())
    if remaining:
        print(
            f"jemalloc-guard: ok — {len(KNOWN_SITES)} file(s), "
            f"{remaining} reference(s) still to remove."
        )
    else:
        print("jemalloc-guard: ok — no jemalloc references remain.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
