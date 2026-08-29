"""Guard the `.config/nextest.toml` filters against silent decay (#1158).

Nextest does not error on a filterset that matches nothing. It is ignored,
the affected tests quietly fall back to the default profile, and the
regression surfaces much later as a flake with nothing pointing at the
config. #1158 moved every top-level `crates/*/tests/*.rs` file into a
category target, which is exactly the kind of rename that leaves a
`binary(...)` or `test(/^module::/)` filter dangling.

This guard resolves every name a filter mentions against the test targets
that actually exist on disk, so a rename that orphans a filter fails lint
instead of passing CI. It is deliberately static: running
`cargo nextest list -E '<filter>'` would prove the same thing but needs a
full test-profile build, which lint cannot afford.

It covers three places a test target or test name can be referenced, because
#1158 broke one of each and the first version of this guard -- which read
only `.config/nextest.toml` -- could not see two of them:

* `.config/nextest.toml` -- `binary(...)` and `test(/^module::/)` in override
  filtersets. A stale one is *silently ignored* by nextest and the test drops
  back to the default budget.
* `ci/*.py` -- `"--test", "<target>"` argv pairs, and `binary(...)` inside
  `-E` filter strings. `ci/test.py` excluded a host-sensitive trybuild suite
  with `not binary(brokered_backend_ui)`; once that binary was gone the
  exclusion excluded *nothing* and the suite would have started running on
  hosts its snapshots were never written for.
* `.github/workflows/*.yml` -- `--test <target>` on a cargo invocation. This
  one fails loudly rather than silently, but it fails a whole lane.

Not covered, and deliberately so: `--exact <test>` self-re-execution, where a
test spawns its own binary to run a helper by name. #1158 broke two of those
too. Resolving them statically would mean knowing every `#[test]` name in the
tree, which needs a build; `tests/core/process_core_test.rs` instead derives
the name from `module_path!()` so it cannot drift, and pins that with a test.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CONFIG = ROOT / ".config" / "nextest.toml"

FILTER_LINE = re.compile(r"^\s*filter\s*=\s*'([^']*)'\s*$", re.MULTILINE)
BINARY_REF = re.compile(r"binary\(([A-Za-z0-9_]+)\)")
# `test(/^module::/)` and `test(/module::/)` — the module-qualified form a
# consolidated target requires.
TEST_MODULE_REF = re.compile(r"test\(/\^?([A-Za-z0-9_]+)::")

# `"--test",` then the target name as the next argv string -- the form
# `ci/*.py` uses when it builds a command as a list.
PY_TEST_TARGET = re.compile(r'"--test"\s*,\s*\n?\s*(?:#[^\n]*\n\s*)*"([A-Za-z0-9_]+)"')
# `--test <target>` on a shell command line, as the workflows spell it.
SH_TEST_TARGET = re.compile(r"--test\s+([A-Za-z0-9_]+)")


def test_targets() -> dict[str, Path]:
    """Every integration-test link target, by the name nextest binds to it."""
    targets: dict[str, Path] = {}
    for tests_dir in ROOT.glob("crates/*/tests"):
        for entry in sorted(tests_dir.iterdir()):
            if entry.is_file() and entry.suffix == ".rs":
                targets[entry.stem] = entry
            elif entry.is_dir() and (entry / "main.rs").is_file():
                targets[entry.name] = entry / "main.rs"
    return targets


def declared_modules(main_rs: Path) -> set[str]:
    text = main_rs.read_text(encoding="utf-8")
    return set(re.findall(r"^\s*mod\s+([A-Za-z0-9_]+)\s*;", text, re.MULTILINE))


def main() -> int:
    if not CONFIG.is_file():
        print(f"nextest-filter-guard: {CONFIG} is missing")
        return 1

    config = CONFIG.read_text(encoding="utf-8")
    filters = FILTER_LINE.findall(config)
    if not filters:
        print("nextest-filter-guard: no `filter = '...'` overrides found")
        return 1

    targets = test_targets()
    errors: list[str] = []

    for expression in filters:
        binaries = BINARY_REF.findall(expression)
        for name in binaries:
            if name not in targets:
                errors.append(
                    f"filter {expression!r} names binary({name}), which is not a "
                    f"test target. Known targets: {', '.join(sorted(targets))}"
                )
        for module in TEST_MODULE_REF.findall(expression):
            # The module must be declared by one of the binaries the same
            # expression selects; otherwise the conjunction is empty.
            owners = [b for b in binaries if b in targets]
            if not owners:
                errors.append(
                    f"filter {expression!r} matches module {module}:: but names no "
                    "resolvable binary(...), so nothing constrains which target it "
                    "comes from"
                )
                continue
            if not any(module in declared_modules(targets[b]) for b in owners):
                errors.append(
                    f"filter {expression!r} matches module {module}::, which none of "
                    f"{owners} declares as a `mod`. The filter selects nothing."
                )

    # Same resolution, applied to the two places outside the nextest config
    # that name a test target. `ci/*.py` is where #1158's silent exclusion
    # lived; the workflows are where a `--test` argument fails a whole lane.
    extra_sources: list[tuple[Path, str]] = []
    for path in sorted(ROOT.glob("ci/*.py")):
        if path.name == Path(__file__).name:
            continue
        extra_sources.append((path, path.read_text(encoding="utf-8")))
    for path in sorted(ROOT.glob(".github/workflows/*.yml")):
        extra_sources.append((path, path.read_text(encoding="utf-8")))

    for path, text in extra_sources:
        rel = path.relative_to(ROOT)
        names = set(PY_TEST_TARGET.findall(text)) if path.suffix == ".py" else set()
        if path.suffix == ".yml":
            names = set(SH_TEST_TARGET.findall(text))
        for name in sorted(names):
            if name not in targets:
                errors.append(
                    f"{rel} selects `--test {name}`, which is not a test target. "
                    f"Known targets: {', '.join(sorted(targets))}"
                )
        for name in sorted(set(BINARY_REF.findall(text))):
            if name not in targets:
                errors.append(
                    f"{rel} names binary({name}) in a filter, which is not a test "
                    "target. A stale `binary(...)` matches nothing -- and inside a "
                    "`not binary(...)` exclusion it excludes nothing, which is worse."
                )

    if errors:
        print("nextest-filter-guard: stale test-target references")
        for error in errors:
            print(f"  - {error}")
        return 1

    print(
        f"nextest-filter-guard: {len(filters)} filterset(s) and every "
        f"--test/binary(...) reference in ci/ and .github/workflows/ resolve "
        "to live targets"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
