"""Validate the platform-boundary ledger and dependency boundary.

This checker intentionally does not rely on Rust module expansion: it walks
every handwritten Rust file under ``crates/`` and validates the exact ledger
that the pre-expansion Dylint emits.  Dylint owns syntax-aware occurrence
classification; this companion owns deterministic ledger and manifest
ratcheting in the normal cross-host lint entrypoint.
"""

from __future__ import annotations

import argparse
import collections
import re
import sys
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LEDGER = ROOT / "lints/running-process-platform-boundary/src/baseline.txt"
MANIFEST_LEDGER = ROOT / "ci/platform_boundary.manifest.tsv"
PLATFORM_CRATE = "crates/running-process-platform-internal"
CONCRETE_PREFIXES = (
    f"{PLATFORM_CRATE}/src/platform_win",
    f"{PLATFORM_CRATE}/src/platform_linux",
    f"{PLATFORM_CRATE}/src/platform_macos",
)
KINDS = {"attr_cfg", "cfg_macro", "native_import", "module_ref"}


@dataclass(frozen=True)
class ArtifactZone:
    """A native component that keeps its host selection, under a stated contract.

    #965 excludes a handful of components from ordinary migration -- signal
    safety, loader ABI, AV/EDR static analysis and link seams all make "move it
    behind the facade" the wrong answer. #974 asks that those exemptions be
    *named zones with contracts* rather than allowlisted paths, so that a zone
    rejects what it was never meant to cover.

    A zone therefore states what its artifact may reference, and everything
    else under its prefix is a failure. That is the difference between "this
    directory is exempt" and "this artifact is allowed exactly these
    mechanics": the second one still catches a Linux import appearing in a
    Windows-only artifact, or ordinary product code moving in to claim the
    exemption.
    """

    name: str
    prefix: str
    reason: str
    host_keys: frozenset[str]
    host_values: frozenset[str]
    native_imports: frozenset[str]


ARTIFACT_ZONES: tuple[ArtifactZone, ...] = (
    ArtifactZone(
        name="win-gnu-bridge",
        prefix="crates/running-process-win-gnu-bridge",
        reason=(
            "The crate exists to prove that the MSVC-obligatory Windows API "
            "surface links under x86_64-pc-windows-gnu (#580). Its host "
            "selection is the thing under test, so moving it behind the facade "
            "would delete the seam rather than relocate it."
        ),
        host_keys=frozenset({"target_os", "target_env"}),
        host_values=frozenset({"windows", "gnu", "msvc"}),
        native_imports=frozenset({"windows_sys"}),
    ),
)

ZONE_PREFIXES = tuple(zone.prefix for zone in ARTIFACT_ZONES)

HOST_KEYS = {
    "windows",
    "unix",
    "target_abi",
    "target_arch",
    "target_endian",
    "target_env",
    "target_family",
    "target_os",
    "target_pointer_width",
    "target_vendor",
}
NATIVE_DEPS = {
    "interprocess",
    "libc",
    "mach2",
    "portable-pty",
    "winapi",
    "windows-sys",
    "windows_sys",
}
# Crates permitted a per-target dependency table. This is the manifest half
# of the same idea as `ARTIFACT_ZONES` above: this set says which crates may
# declare per-target dependencies at all, and a zone says exactly what the
# crate's *sources* may then reference. A crate with a zone should appear
# here too, which `zone_manifest_alignment_violations` checks.
SPECIALIZED_ARTIFACTS = {
    "running-process-probe-interposer-linux",
    "running-process-probe-interposer-macos",
    "running-process-probe-interposer-windows",
    "running-process-win-gnu-bridge",
}
TARGET_TABLE = re.compile(r"^\s*\[target\..+\.dependencies\]\s*$", re.MULTILINE)
DEPENDENCY_KEY = re.compile(r"^\s*([A-Za-z0-9_-]+)\s*=", re.MULTILINE)
ATTRIBUTE_CFG = re.compile(r"#\s*\[\s*cfg(?:_attr)?\s*\((.*?)\)\s*\]", re.DOTALL)
CFG_MACRO = re.compile(r"\bcfg\s*!\s*\((.*?)\)", re.DOTALL)
IDENTIFIER = re.compile(r"\b[A-Za-z_][A-Za-z0-9_]*\b")
NATIVE_PATH = re.compile(r"\bstd\s*::\s*os\s*::\s*(unix|windows)\b")
# `target_os = "linux"` and friends. The key alone does not say which
# platform an artifact is for, so a zone contract has to read the value.
HOST_VALUE = re.compile(r"\b([a-z_]+)\s*=\s*\"([A-Za-z0-9_.-]+)\"")
NATIVE_ROOT = re.compile(r"\b(libc|windows_sys)\s*::")
CONCRETE_MODULE = re.compile(
    r"\b(platform_win|platform_linux|platform_macos|platform_imp)\b"
)
RAW_PTY_CONTROL_PAYLOAD = re.compile(
    r"\b(?:PtyMasterControlToken|PtyChildControlToken)\b"
    r"|\b(?:raw_fd|raw_handle|process_group_leader)\s*:"
)
NEUTRAL_TERMINAL_FACADE = ROOT / PLATFORM_CRATE / "src" / "platform" / "terminal.rs"


@dataclass(frozen=True, order=True)
class Row:
    path: str
    kind: str
    normalized: str
    ordinal: int


def err(message: str) -> None:
    print(f"platform-boundary: {message}", file=sys.stderr)


def source_files() -> set[str]:
    """Return every handwritten crate Rust file, including otherwise orphaned files."""
    return {
        path.relative_to(ROOT).as_posix()
        for path in (ROOT / "crates").rglob("*.rs")
        if "target" not in path.parts
    }


def production_source_files() -> set[str]:
    """Return source files covered by the bootstrap ledger's current scope."""
    files = source_files()
    return {
        path
        for path in files
        if "/src/" in path
        and path != f"{PLATFORM_CRATE}/src/lib.rs"
        and not path.startswith(CONCRETE_PREFIXES)
        and not path.startswith(ZONE_PREFIXES)
    }


def code_only(text: str, *, keep_strings: bool = False) -> str:
    """Remove Rust comments and quoted literals without changing token order.

    The implementation deliberately preserves newlines and emits spaces for
    removed bytes, making it a lightweight lexer rather than a regex over raw
    source. Raw strings are handled conservatively; uncertain syntax is left
    unchanged for Dylint to make the authoritative decision.

    ``keep_strings`` retains literal contents while still removing comments.
    Occurrence counting does not want them -- a variable name inside a doc
    comment is not a host mechanic -- but a zone contract is written in terms
    of the *values* a cfg names, and those live inside the quotes.
    """
    out: list[str] = []
    index = 0
    while index < len(text):
        pair = text[index : index + 2]
        if pair == "//":
            end = text.find("\n", index)
            end = len(text) if end < 0 else end
            out.append(" " * (end - index))
            index = end
        elif pair == "/*":
            end = text.find("*/", index + 2)
            end = len(text) - 2 if end < 0 else end
            removed = text[index : end + 2]
            out.append("".join("\n" if char == "\n" else " " for char in removed))
            index = end + 2
        elif text[index] == '"':
            end = index + 1
            while end < len(text):
                if text[end] == "\\":
                    end += 2
                    continue
                if text[end] == '"':
                    end += 1
                    break
                end += 1
            out.append(text[index:end] if keep_strings else " " * (end - index))
            index = end
        else:
            out.append(text[index])
            index += 1
    return "".join(out)


def scan_source(path: str) -> collections.Counter[tuple[str, str, str]]:
    """Find the Dylint-equivalent, syntax-independent bootstrap subset."""
    text = code_only((ROOT / path).read_text(encoding="utf-8"))
    found: collections.Counter[tuple[str, str, str]] = collections.Counter()
    for match in ATTRIBUTE_CFG.finditer(text):
        for identifier in IDENTIFIER.findall(match.group(1)):
            if identifier in HOST_KEYS:
                found[(path, "attr_cfg", identifier)] += 1
    for match in CFG_MACRO.finditer(text):
        for identifier in IDENTIFIER.findall(match.group(1)):
            if identifier in HOST_KEYS:
                found[(path, "cfg_macro", identifier)] += 1
    for match in NATIVE_PATH.finditer(text):
        found[(path, "native_import", f"std::os::{match.group(1)}")] += 1
    for match in NATIVE_ROOT.finditer(text):
        found[(path, "native_import", match.group(1))] += 1
    for match in CONCRETE_MODULE.finditer(text):
        found[(path, "module_ref", match.group(1))] += 1
    return found


def source_scan_violations(rows: list[Row]) -> list[str]:
    """Reject locally-scannable debt growth before the nightly Dylint lane."""
    allowed: collections.Counter[tuple[str, str, str]] = collections.Counter(
        (row.path, row.kind, row.normalized) for row in rows
    )
    observed: collections.Counter[tuple[str, str, str]] = collections.Counter()
    for path in sorted(production_source_files()):
        observed.update(scan_source(path))
    failures: list[str] = []
    for key, count in sorted(observed.items()):
        if count > allowed[key]:
            failures.append(
                f"new locally-scanned occurrence ({count - allowed[key]}): {' '.join(key)}"
            )
    return failures


def parse_ledger(path: Path = LEDGER) -> list[Row]:
    rows: list[Row] = []
    for line_number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not raw or raw.startswith("#"):
            continue
        fields = raw.split("\t")
        if len(fields) != 4:
            raise ValueError(
                f"{path.relative_to(ROOT)}:{line_number}: expected four tab-separated fields"
            )
        source, kind, normalized, ordinal_text = fields
        if kind not in KINDS:
            raise ValueError(
                f"{path.relative_to(ROOT)}:{line_number}: unknown kind {kind!r}"
            )
        try:
            ordinal = int(ordinal_text)
        except ValueError as exc:
            raise ValueError(
                f"{path.relative_to(ROOT)}:{line_number}: ordinal is not an integer"
            ) from exc
        if ordinal < 0:
            raise ValueError(
                f"{path.relative_to(ROOT)}:{line_number}: ordinal is negative"
            )
        rows.append(Row(source, kind, normalized, ordinal))
    return rows


def validate_ledger(rows: list[Row]) -> list[str]:
    failures: list[str] = []
    if not rows:
        return [
            "ledger is empty; an empty ledger is only valid in the final consolidation phase"
        ]
    all_sources = source_files()
    grouped: dict[tuple[str, str, str], list[int]] = collections.defaultdict(list)
    for row in rows:
        grouped[(row.path, row.kind, row.normalized)].append(row.ordinal)
        if row.path not in all_sources:
            failures.append(f"stale or out-of-scope row: {row.path}")
        if row.path == f"{PLATFORM_CRATE}/src/lib.rs" or row.path.startswith(
            CONCRETE_PREFIXES
        ):
            failures.append(f"allowed-zone row must be removed: {row.path}")
    for key, ordinals in sorted(grouped.items()):
        expected = list(range(len(ordinals)))
        actual = sorted(ordinals)
        if actual != expected:
            failures.append(
                f"non-contiguous or duplicate ordinals for {key[0]} {key[1]} {key[2]}: {actual}"
            )
    return failures


def manifest_occurrences() -> collections.Counter[tuple[str, str, str]]:
    """Inventory legacy manifest boundary debt with exact occurrence counts."""
    occurrences: collections.Counter[tuple[str, str, str]] = collections.Counter()
    for manifest in sorted((ROOT / "crates").glob("*/Cargo.toml")):
        crate = manifest.parent.name
        text = manifest.read_text(encoding="utf-8")
        relative = manifest.parent.name
        if crate in {"running-process-platform-internal", *SPECIALIZED_ARTIFACTS}:
            continue
        if crate != "running-process-platform-internal" and TARGET_TABLE.search(text):
            occurrences[(relative, "target_dependency_table", "target")] += len(
                TARGET_TABLE.findall(text)
            )
        for dependency in DEPENDENCY_KEY.findall(text):
            if dependency in NATIVE_DEPS:
                occurrences[(relative, "native_dependency", dependency)] += 1
    return occurrences


def parse_manifest_ledger() -> collections.Counter[tuple[str, str, str]]:
    expected: collections.Counter[tuple[str, str, str]] = collections.Counter()
    for line_number, raw in enumerate(
        MANIFEST_LEDGER.read_text(encoding="utf-8").splitlines(), 1
    ):
        if not raw or raw.startswith("#"):
            continue
        fields = raw.split("\t")
        if len(fields) != 3:
            location = MANIFEST_LEDGER.relative_to(ROOT)
            raise ValueError(
                f"{location}:{line_number}: expected three tab-separated fields"
            )
        expected[tuple(fields)] += 1
    return expected


def manifest_dependency_violations() -> list[str]:
    expected = parse_manifest_ledger()
    observed = manifest_occurrences()
    failures: list[str] = []
    for row, count in sorted(observed.items()):
        if count > expected[row]:
            failures.append(
                f"new manifest boundary occurrence ({count - expected[row]}): {' '.join(row)}"
            )
    for row, count in sorted(expected.items()):
        if count > observed[row]:
            failures.append(
                f"stale manifest boundary occurrence ({count - observed[row]}): {' '.join(row)}"
            )
    return failures


def neutral_facade_contract_violations() -> list[str]:
    """Reject raw PTY control payloads disguised as facade-owned tokens."""
    text = code_only(NEUTRAL_TERMINAL_FACADE.read_text(encoding="utf-8"))
    return [
        "neutral PTY facade carries raw descriptor/handle control payloads"
        for _match in RAW_PTY_CONTROL_PAYLOAD.finditer(text)
    ]


def zone_for(path: str) -> ArtifactZone | None:
    """Return the zone covering ``path``, if any."""
    for zone in ARTIFACT_ZONES:
        if path.startswith(zone.prefix):
            return zone
    return None


def artifact_zone_violations() -> list[str]:
    """Hold each zone to its own contract.

    An exempt path is not a licence to reference anything. A zone names the
    host keys, the values and the native crates its artifact is allowed to
    reach for; anything else under its prefix fails here, which is what stops
    an exemption from widening quietly once it exists.
    """
    failures: list[str] = []
    for zone in ARTIFACT_ZONES:
        covered = sorted(
            path
            for path in source_files()
            if path.startswith(zone.prefix) and "/src/" in path
        )
        if not covered:
            failures.append(
                f"zone {zone.name!r} covers no sources; a zone with nothing in "
                "it is a stale exemption, not a contract"
            )
            continue
        for path in covered:
            failures.extend(
                zone_text_violations(
                    zone, path, (ROOT / path).read_text(encoding="utf-8")
                )
            )
    return failures


def zone_text_violations(zone: ArtifactZone, path: str, text: str) -> list[str]:
    """Hold one source text to one zone's contract.

    Kept pure -- text in, findings out -- so the contract can be exercised
    without writing to the tree being checked. A test that had to edit a real
    source file to prove a rule would be unsafe beside a parallel suite, and
    would leave the repository dirty if it failed part-way.
    """
    failures: list[str] = []
    code = code_only(text)
    counted: collections.Counter[tuple[str, str, str]] = collections.Counter()
    for match in ATTRIBUTE_CFG.finditer(code):
        for identifier in IDENTIFIER.findall(match.group(1)):
            if identifier in HOST_KEYS:
                counted[(path, "attr_cfg", identifier)] += 1
    for match in CFG_MACRO.finditer(code):
        for identifier in IDENTIFIER.findall(match.group(1)):
            if identifier in HOST_KEYS:
                counted[(path, "cfg_macro", identifier)] += 1
    for match in NATIVE_PATH.finditer(code):
        counted[(path, "native_import", f"std::os::{match.group(1)}")] += 1
    for match in NATIVE_ROOT.finditer(code):
        counted[(path, "native_import", match.group(1))] += 1
    for match in CONCRETE_MODULE.finditer(code):
        counted[(path, "module_ref", match.group(1))] += 1

    for (_, kind, construct), _ in sorted(counted.items()):
        if kind == "native_import":
            if construct not in zone.native_imports:
                failures.append(
                    f"zone {zone.name!r} does not permit native import "
                    f"{construct!r}: {path}"
                )
        elif kind in {"attr_cfg", "cfg_macro"}:
            if construct not in zone.host_keys:
                failures.append(
                    f"zone {zone.name!r} does not permit host key "
                    f"{construct!r}: {path}"
                )
        else:
            failures.append(
                f"zone {zone.name!r} does not permit {kind} {construct!r}: {path}"
            )

    # Values, not just keys: checking keys alone would let a Windows-only
    # artifact grow a `target_os = "linux"` arm, since the key is the same.
    # The value is where "this artifact is for one platform" is written down.
    for match in HOST_VALUE.finditer(code_only(text, keep_strings=True)):
        key, value = match.group(1), match.group(2)
        if key in zone.host_keys and value not in zone.host_values:
            failures.append(
                f"zone {zone.name!r} does not permit {key} = {value!r}: {path}"
            )
    return failures


def zone_manifest_alignment_violations() -> list[str]:
    """A zone's crate must also be allowed its per-target dependency table.

    The two lists answer different questions -- one about manifests, one about
    sources -- but they are claims about the same crates. Letting them drift
    would mean a crate whose sources are held to a contract while its manifest
    is not, or the reverse, and neither half would notice.
    """
    failures: list[str] = []
    for zone in ARTIFACT_ZONES:
        crate = zone.prefix.split("/")[-1]
        if crate not in SPECIALIZED_ARTIFACTS:
            failures.append(
                f"zone {zone.name!r} covers {crate!r}, which is not in "
                "SPECIALIZED_ARTIFACTS; the manifest and source halves must "
                "name the same crates"
            )
    return failures


def totals(rows: list[Row]) -> str:
    by_kind = collections.Counter(row.kind for row in rows)
    by_crate = collections.Counter(row.path.split("/")[1] for row in rows)
    kinds = ", ".join(f"{kind}={by_kind[kind]}" for kind in sorted(by_kind))
    crates = ", ".join(f"{crate}={by_crate[crate]}" for crate in sorted(by_crate))
    return f"rows={len(rows)}; kinds: {kinds}; crates: {crates}"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--print-totals", action="store_true")
    args = parser.parse_args(argv)
    try:
        rows = parse_ledger()
    except (OSError, ValueError) as exc:
        err(str(exc))
        return 1
    failures = [
        *validate_ledger(rows),
        *source_scan_violations(rows),
        *manifest_dependency_violations(),
        *neutral_facade_contract_violations(),
        *artifact_zone_violations(),
        *zone_manifest_alignment_violations(),
    ]
    if args.print_totals:
        print(totals(rows))
    for failure in failures:
        err(failure)
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
