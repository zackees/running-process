"""Sync/async parity manifest gate for #875.

`docs/async_api_parity.toml` is the checked-in inventory of every public
operation on the five process surfaces this library ships. This module is what
makes that inventory load-bearing instead of decorative:

* **Coverage.** Every public member discovered in the source of a tracked
  surface must have a manifest row, and every row must name a member that
  still exists. Adding a public sync method without a parity row fails lint,
  which is the drift guard the markdown table could never be.
* **Evidence.** A row marked ``implemented`` must name a test per applicable
  column, and each named test must actually exist in the tree. A column that
  genuinely does not apply carries ``n/a: <rationale>`` instead.
* **RED accounting.** A row marked ``planned`` is outstanding work. It is
  reported on every run so the gap is visible, and it must cite the issue that
  owns it. When ``require_no_planned`` flips to ``true`` in the manifest, any
  remaining planned row becomes a hard failure -- that is the switch that
  closes #875.

Discovery is static (``ast`` for Python, a brace scanner for Rust) so the gate
runs without importing the package or building the native extension.
"""

from __future__ import annotations

import argparse
import ast
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "docs" / "async_api_parity.toml"
DOCUMENT = ROOT / "docs" / "ASYNC_API_PARITY.md"

GENERATED_BEGIN = "<!-- BEGIN GENERATED PARITY TABLE -->"
GENERATED_END = "<!-- END GENERATED PARITY TABLE -->"

COLUMNS = ("rust_sync", "rust_async", "python_sync", "python_async")
STATUSES = ("implemented", "planned")
NA_PREFIX = "n/a:"


@dataclass(frozen=True)
class Surface:
    """A tracked class whose public members must all appear in the manifest."""

    key: str
    title: str
    language: str
    source: Path
    symbol: str


SURFACES: tuple[Surface, ...] = (
    Surface(
        key="rust-process",
        title="Rust `NativeProcess`",
        language="rust",
        source=Path("crates/running-process/src/lib.rs"),
        symbol="NativeProcess",
    ),
    Surface(
        key="rust-pty",
        title="Rust `NativePtyProcess`",
        language="rust",
        source=Path("crates/running-process/src/pty/native_pty_process.rs"),
        symbol="NativePtyProcess",
    ),
    Surface(
        key="python-process",
        title="Python `RunningProcess`",
        language="python",
        source=Path("src/running_process/running_process/_core.py"),
        symbol="RunningProcess",
    ),
    Surface(
        key="python-pty",
        title="Python `PseudoTerminalProcess`",
        language="python",
        source=Path("src/running_process/pty/_pseudo_terminal.py"),
        symbol="PseudoTerminalProcess",
    ),
    Surface(
        key="python-interactive",
        title="Python `InteractiveProcess`",
        language="python",
        source=Path("src/running_process/pty/_interactive.py"),
        symbol="InteractiveProcess",
    ),
    Surface(
        key="python-helpers",
        title="Python module-level helpers",
        language="python-module",
        source=Path("src/running_process/__init__.py"),
        symbol="running_process",
    ),
)

SURFACES_BY_KEY = {surface.key: surface for surface in SURFACES}

# Module-level helpers are re-exported names, so `__all__` alone would sweep in
# every dataclass and enum the package publishes. The parity question is only
# about callables that *do something to a process*, which is this set.
MODULE_HELPERS: frozenset[str] = frozenset(
    {
        "find_processes_by_originator",
        "get_process_tree_info",
        "kill_process_tree",
        "launch_detached",
        "subprocess_run",
        "terminate_process_tree",
    }
)


@dataclass
class Row:
    identifier: str
    surface: str
    member: str
    status: str
    issue: str = ""
    note: str = ""
    columns: dict[str, str] = field(default_factory=dict)


@dataclass
class Manifest:
    require_no_planned: bool
    rows: list[Row]


# --------------------------------------------------------------------------
# manifest parsing
# --------------------------------------------------------------------------


def _scalar(raw: str) -> str | bool:
    raw = raw.strip()
    if raw in {"true", "false"}:
        return raw == "true"
    if len(raw) >= 2 and raw[0] == '"' and raw[-1] == '"':
        return raw[1:-1]
    raise ValueError(f"unsupported manifest value: {raw!r}")


def parse_manifest(text: str) -> Manifest:
    """Parse the small TOML subset the manifest uses.

    Deliberately hand-rolled rather than `tomllib`: this repo supports Python
    3.10, where `tomllib` does not exist, and the sibling
    `ci.async_compliance_guard` gate already reads its policy file the same way.
    """
    require_no_planned = False
    raw_rows: list[dict[str, str | bool]] = []
    current: dict[str, str | bool] | None = None
    section = ""
    for lineno, raw_line in enumerate(text.splitlines(), start=1):
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if line == "[[row]]":
            current = {}
            raw_rows.append(current)
            section = "row"
            continue
        if line.startswith("["):
            section = line.strip("[]")
            current = None
            continue
        if "=" not in line:
            raise ValueError(f"{MANIFEST.name}:{lineno}: expected `key = value`")
        key, value = line.split("=", 1)
        key = key.strip()
        parsed = _scalar(value)
        if section == "settings":
            if key == "require_no_planned":
                require_no_planned = bool(parsed)
            else:
                raise ValueError(f"{MANIFEST.name}:{lineno}: unknown setting {key}")
            continue
        if current is None:
            raise ValueError(f"{MANIFEST.name}:{lineno}: {key} outside of a [[row]]")
        current[key] = parsed

    rows = [
        Row(
            identifier=str(raw.get("id", "")),
            surface=str(raw.get("surface", "")),
            member=str(raw.get("member", "")),
            status=str(raw.get("status", "")),
            issue=str(raw.get("issue", "")),
            note=str(raw.get("note", "")),
            columns={name: str(raw.get(name, "")) for name in COLUMNS},
        )
        for raw in raw_rows
    ]
    return Manifest(require_no_planned=require_no_planned, rows=rows)


# --------------------------------------------------------------------------
# surface discovery
# --------------------------------------------------------------------------


def _rust_members(source: str, symbol: str) -> set[str]:
    """Public method names declared directly inside `impl <symbol> { ... }`.

    Indentation is the delimiter, not brace depth: `cargo fmt --check` already
    gates this repo, so an impl body is exactly one level in and its closing
    brace is exactly `}` at column zero. Counting braces would instead have to
    reason about every `{}` inside a `format!` string in a 1000-line impl.
    """
    marker = re.search(rf"^impl {re.escape(symbol)}\s*\{{$", source, re.MULTILINE)
    if marker is None:
        raise ValueError(f"no `impl {symbol}` block found")
    members: set[str] = set()
    body = source[marker.end() :].splitlines()
    for line in body:
        if line == "}":
            break
        # Bare `pub` only. `pub(crate)` and `pub(super)` are internal plumbing,
        # not surface a downstream consumer can call, so they owe no parity row.
        found = re.match(r"    pub\s+(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)", line)
        if found is not None and not found.group(1).startswith("_"):
            members.add(found.group(1))
    else:
        raise ValueError(f"`impl {symbol}` block is not terminated at column zero")
    return members


def _python_members(source: str, symbol: str) -> set[str]:
    tree = ast.parse(source)
    for node in tree.body:
        if isinstance(node, ast.ClassDef) and node.name == symbol:
            return {
                child.name
                for child in node.body
                if isinstance(child, ast.FunctionDef | ast.AsyncFunctionDef)
                and not child.name.startswith("_")
            }
    raise ValueError(f"no `class {symbol}` found")


def discover(surface: Surface) -> set[str]:
    source = (ROOT / surface.source).read_text(encoding="utf-8")
    if surface.language == "rust":
        return _rust_members(source, surface.symbol)
    if surface.language == "python":
        return _python_members(source, surface.symbol)
    exported = set(re.findall(r'^\s*"(\w+)",', source, re.MULTILINE))
    return exported & MODULE_HELPERS


# --------------------------------------------------------------------------
# test index
# --------------------------------------------------------------------------


def _rust_tests() -> set[str]:
    names: set[str] = set()
    for path in ROOT.glob("crates/**/*.rs"):
        if "target" in path.parts:
            continue
        attributed = False
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            stripped = line.strip()
            if stripped.startswith("#["):
                attributed = attributed or "test" in stripped
                continue
            found = re.match(r"(?:async\s+)?fn\s+(\w+)", stripped)
            if found is not None and attributed:
                names.add(found.group(1))
            attributed = False
    return names


def _python_tests() -> set[str]:
    names: set[str] = set()
    for path in ROOT.glob("tests/**/*.py"):
        try:
            tree = ast.parse(path.read_text(encoding="utf-8"))
        except SyntaxError:  # pragma: no cover - a broken test file fails pytest
            continue
        for node in ast.walk(tree):
            if isinstance(node, ast.FunctionDef | ast.AsyncFunctionDef):
                if node.name.startswith("test_"):
                    names.add(node.name)
    return names


# --------------------------------------------------------------------------
# checking
# --------------------------------------------------------------------------


def check(strict: bool = False) -> tuple[list[str], list[str]]:
    """Return (failures, outstanding-planned-rows).

    `strict` forces the end-state gate on regardless of the manifest setting.
    That is how a slice demonstrates its RED state: run with `--strict` before
    the change and every not-yet-async operation is reported as a failure.
    """
    failures: list[str] = []
    try:
        manifest = parse_manifest(MANIFEST.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        return [str(error)], []

    seen_ids: set[str] = set()
    by_surface: dict[str, set[str]] = {}
    for row in manifest.rows:
        label = row.identifier or "<unnamed>"
        if not row.identifier:
            failures.append("a [[row]] is missing `id`")
        elif row.identifier in seen_ids:
            failures.append(f"{label}: duplicate row id")
        seen_ids.add(row.identifier)
        if row.surface not in SURFACES_BY_KEY:
            failures.append(f"{label}: unknown surface {row.surface!r}")
            continue
        if not row.member:
            failures.append(f"{label}: missing `member`")
            continue
        by_surface.setdefault(row.surface, set()).add(row.member)
        if row.status not in STATUSES:
            failures.append(
                f"{label}: status must be one of {', '.join(STATUSES)}, got {row.status!r}"
            )
            continue
        if row.status == "planned" and not row.issue:
            failures.append(f"{label}: a planned row must cite the owning `issue`")

    rust_tests = _rust_tests()
    python_tests = _python_tests()
    for row in manifest.rows:
        for column in COLUMNS:
            value = row.columns.get(column, "")
            if not value:
                # A planned row is allowed to have empty columns -- that is what
                # planned means. An implemented row is not.
                if row.status == "implemented":
                    failures.append(
                        f"{row.identifier}: implemented row is missing {column}"
                    )
                continue
            if value.startswith(NA_PREFIX):
                if not value[len(NA_PREFIX) :].strip():
                    failures.append(
                        f"{row.identifier}: {column} is marked n/a without a rationale"
                    )
                continue
            known = rust_tests if column.startswith("rust_") else python_tests
            if value not in known:
                failures.append(
                    f"{row.identifier}: {column} names test {value!r}, which does not "
                    "exist in the tree"
                )

    for surface in SURFACES:
        try:
            discovered = discover(surface)
        except (OSError, ValueError) as error:
            failures.append(f"{surface.key}: {error}")
            continue
        claimed = by_surface.get(surface.key, set())
        for missing in sorted(discovered - claimed):
            failures.append(
                f"{surface.key}: public member {missing!r} has no parity row; add one "
                f"to {MANIFEST.relative_to(ROOT).as_posix()}"
            )
        for stale in sorted(claimed - discovered):
            failures.append(
                f"{surface.key}: parity row claims member {stale!r}, which no longer "
                "exists on that surface"
            )

    planned = [row for row in manifest.rows if row.status == "planned"]
    if manifest.require_no_planned or strict:
        for row in planned:
            failures.append(
                f"{row.identifier}: no async parity contract yet (planned, #{row.issue})"
            )

    document_failures = _check_document(manifest)
    failures.extend(document_failures)
    return failures, [f"{row.identifier} (#{row.issue})" for row in planned]


def render_table(manifest: Manifest) -> str:
    lines = [GENERATED_BEGIN, ""]
    lines.append(
        "This table is generated from `docs/async_api_parity.toml` by "
        "`ci.parity_manifest`. Edit the manifest, then run "
        "`uv run --no-sync python -m ci.parity_manifest --write`."
    )
    for surface in SURFACES:
        rows = [row for row in manifest.rows if row.surface == surface.key]
        if not rows:
            continue
        lines.extend(["", f"### {surface.title}", ""])
        lines.append(
            "| Member | Status | Rust sync | Rust async | Python sync | Python async |"
        )
        lines.append("| --- | --- | --- | --- | --- | --- |")
        for row in sorted(rows, key=lambda item: item.member):
            cells = [f"`{row.member}`", row.status]
            for column in COLUMNS:
                value = row.columns.get(column, "")
                if not value:
                    cells.append("-")
                elif value.startswith(NA_PREFIX):
                    cells.append(value)
                else:
                    cells.append(f"`{value}`")
            lines.append("| " + " | ".join(cells) + " |")
    lines.extend(["", GENERATED_END])
    return "\n".join(lines)


def _check_document(manifest: Manifest) -> list[str]:
    try:
        document = DOCUMENT.read_text(encoding="utf-8")
    except OSError as error:
        return [str(error)]
    expected = render_table(manifest)
    start = document.find(GENERATED_BEGIN)
    end = document.find(GENERATED_END)
    if start == -1 or end == -1:
        return [
            f"{DOCUMENT.name} is missing the generated parity table markers; run "
            "`python -m ci.parity_manifest --write`"
        ]
    actual = document[start : end + len(GENERATED_END)]
    if actual != expected:
        return [
            f"{DOCUMENT.name} is out of date with the manifest; run "
            "`python -m ci.parity_manifest --write`"
        ]
    return []


def write_document() -> None:
    manifest = parse_manifest(MANIFEST.read_text(encoding="utf-8"))
    document = DOCUMENT.read_text(encoding="utf-8")
    expected = render_table(manifest)
    start = document.find(GENERATED_BEGIN)
    end = document.find(GENERATED_END)
    if start == -1 or end == -1:
        document = document.rstrip("\n") + "\n\n" + expected + "\n"
    else:
        document = document[:start] + expected + document[end + len(GENERATED_END) :]
    DOCUMENT.write_text(document, encoding="utf-8")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--write",
        action="store_true",
        help="regenerate the table in docs/ASYNC_API_PARITY.md from the manifest",
    )
    parser.add_argument(
        "--strict",
        action="store_true",
        help="fail on every planned row -- the RED view of the remaining work",
    )
    args = parser.parse_args(argv)
    if args.write:
        write_document()
        print("parity manifest: regenerated docs/ASYNC_API_PARITY.md")
        return 0

    failures, planned = check(strict=args.strict)
    if planned:
        print(f"parity manifest: {len(planned)} row(s) still planned (RED):")
        for entry in planned:
            print(f"  {entry}")
    if failures:
        print("parity manifest gate failed:")
        for failure in failures:
            print(f"  {failure}")
        return 1
    print("parity manifest gate passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
