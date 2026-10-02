"""Fail when a workflow cache step can save from a pull_request run (#1216).

A cache saved on ``refs/pull/N/merge`` is only restorable by later runs of that
same PR, yet it counts against the repository's 10 GB limit and evicts
``main``'s entries. Every cache *save* must therefore be gated to ``main``.

Rules, applied to every step in ``.github/workflows/*.yml``:

* ``Swatinem/rust-cache`` is banned outright (fleet rule CACHE-025,
  zackees/ci.yml#209): Rust build caching goes through
  ``zackees/setup-soldr``.
* ``zackees/setup-soldr`` must leave ``save-cache`` unset (its ``auto``
  default never saves on ``pull_request``), or set it to ``auto``,
  ``false``, or an expression that names ``refs/heads/main``.
* ``actions/cache`` (restore + save) is forbidden: use
  ``actions/cache/restore`` and a separate ``actions/cache/save``.
* ``actions/cache/save`` must carry an ``if:`` gated the same way.

A step may opt out with a comment line ``# cache-save-exempt: <reason>``
directly inside or immediately above the step. The reason is required.

The check is text based on purpose: exemptions live in YAML comments, which a
YAML parser discards.
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github" / "workflows"

EXEMPT = re.compile(r"#\s*cache-save-exempt:\s*\S")
USES = re.compile(r"^(?P<indent>\s*)(?:-\s+)?uses:\s*['\"]?(?P<action>[^@'\"\s]+)@")
STEP_START = re.compile(r"^(?P<indent>\s*)-\s")
MAIN_GATE = re.compile(
    r"refs/heads/main|event_name\s*!=\s*'pull_request'|^\s*['\"]?false['\"]?\s*$"
)

# setup-soldr's own ``auto`` mode skips saves on pull_request (setup-soldr#527).
SOLDR_SAVE_GATE = re.compile(
    r"refs/heads/main|event_name\s*!=\s*'pull_request'"
    r"|^\s*['\"]?(?:false|auto)['\"]?\s*$"
)


@dataclass(frozen=True)
class Violation:
    path: Path
    line: int
    message: str

    def __str__(self) -> str:
        try:
            shown = self.path.relative_to(ROOT)
        except ValueError:
            shown = self.path
        return f"{shown}:{self.line}: {self.message}"


def _step_bounds(lines: list[str], uses_index: int) -> tuple[int, int]:
    """Return [start, end) of the YAML list item containing ``uses_index``."""
    start = uses_index
    while start >= 0 and not STEP_START.match(lines[start]):
        start -= 1
    if start < 0:
        return uses_index, uses_index + 1
    indent = len(STEP_START.match(lines[start]).group("indent"))
    end = start + 1
    while end < len(lines):
        line = lines[end]
        if line.strip() and not line.lstrip().startswith("#"):
            current = len(line) - len(line.lstrip())
            if current <= indent:
                break
        end += 1
    return start, end


def _field(step: list[str], name: str) -> str | None:
    pattern = re.compile(rf"^\s*(?:-\s+)?{re.escape(name)}:\s*(?P<value>.*)$")
    for line in step:
        match = pattern.match(line)
        if match:
            return match.group("value").strip()
    return None


def _exempt(lines: list[str], start: int, end: int) -> bool:
    above = start - 1
    while above >= 0 and lines[above].lstrip().startswith("#"):
        if EXEMPT.search(lines[above]):
            return True
        above -= 1
    return any(EXEMPT.search(line) for line in lines[start:end])


def check_text(path: Path, text: str) -> list[Violation]:
    lines = text.splitlines()
    violations: list[Violation] = []
    for index, line in enumerate(lines):
        match = USES.match(line)
        if not match:
            continue
        action = match.group("action")
        if action not in {
            "Swatinem/rust-cache",
            "zackees/setup-soldr",
            "actions/cache",
            "actions/cache/save",
        }:
            continue
        start, end = _step_bounds(lines, index)
        if _exempt(lines, start, end):
            continue
        step = lines[start:end]
        if action == "Swatinem/rust-cache":
            violations.append(
                Violation(
                    path,
                    index + 1,
                    "Swatinem/rust-cache is banned (CACHE-025, zackees/ci.yml#209); "
                    "use zackees/setup-soldr@v0 and `soldr cargo`",
                )
            )
        elif action == "zackees/setup-soldr":
            gate = _field(step, "save-cache")
            if gate is not None and not SOLDR_SAVE_GATE.search(gate):
                violations.append(
                    Violation(
                        path,
                        index + 1,
                        "zackees/setup-soldr save-cache can save on pull_request; "
                        "leave it unset or gate it on refs/heads/main",
                    )
                )
        elif action == "actions/cache":
            violations.append(
                Violation(
                    path,
                    index + 1,
                    "actions/cache saves on pull_request; split into "
                    "actions/cache/restore + a main-gated actions/cache/save",
                )
            )
        else:
            gate = _field(step, "if")
            if gate is None or not MAIN_GATE.search(gate):
                violations.append(
                    Violation(
                        path,
                        index + 1,
                        "actions/cache/save needs "
                        "`if: github.ref == 'refs/heads/main'`",
                    )
                )
    return violations


def check_paths(paths: list[Path]) -> list[Violation]:
    violations: list[Violation] = []
    for path in paths:
        violations.extend(check_text(path, path.read_text(encoding="utf-8")))
    return violations


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    paths = [Path(arg) for arg in args] or sorted(
        [*WORKFLOWS.glob("*.yml"), *WORKFLOWS.glob("*.yaml")]
    )
    violations = check_paths(paths)
    for violation in violations:
        print(violation, file=sys.stderr)
    if violations:
        print(
            f"cache_save_guard: {len(violations)} cache step(s) can save on "
            "pull_request (#1216)",
            file=sys.stderr,
        )
        return 1
    print(f"cache_save_guard: {len(paths)} workflow(s) OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
