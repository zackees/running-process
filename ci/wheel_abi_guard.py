"""Keep every published wheel installable on every supported interpreter.

The wheels this project ships are ABI3 (`abi3-py310`), so one wheel per
platform covers CPython 3.10 and newer. #1189 dropped that setting to give
Windows an interpreter-tagged extension for #1142, which silently collapsed
*every* platform to a single `cp311-cp311` wheel: 4.10.10 shipped nothing a
macOS/Linux/Windows user on 3.10, 3.12 or 3.13 could install, and pip fell back
to a source build. The free-threading hazard #1142 reported is handled where it
actually bites — at import time, in `running_process._abi_guard` — so packaging
does not have to trade away five interpreter versions for it.

This guard enforces the packaging half of that contract:

* the source configuration still asks maturin and PyO3 for ABI3, and
* every wheel produced carries the `abi3` ABI tag and exactly one ABI3-named
  native extension, and
* a release publishes a wheel for every platform the release cycle builds.
"""

from __future__ import annotations

import argparse
import re
import zipfile
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parent.parent
PYPROJECT = ROOT / "pyproject.toml"
CARGO_MANIFEST = ROOT / "Cargo.toml"

ABI3_FEATURE = "pyo3/abi3-py310"

# The platform tags `auto-release.yml` builds a wheel job for. A release that
# is missing one of these is the 4.10.10 regression happening again, so the
# publish step fails instead of shipping a partial matrix.
RELEASE_PLATFORM_TAGS: tuple[str, ...] = (
    "macosx_10_12_x86_64",
    "macosx_11_0_arm64",
    "manylinux_2_28_x86_64",
    "manylinux_2_28_aarch64",
    "win_amd64",
    "win_arm64",
)

# ABI3 extension file names, per platform. Windows has no `.abi3` infix.
ABI3_EXTENSION_NAMES = ("_native.abi3.so", "_native.pyd", "_native.abi3.dylib")


def validate_source_configuration() -> None:
    """Require ABI3 in the maturin and workspace PyO3 settings."""
    pyproject = tomllib.loads(PYPROJECT.read_text(encoding="utf-8"))
    features = pyproject["tool"]["maturin"].get("features", [])
    if "pyo3/extension-module" not in features:
        raise RuntimeError("[tool.maturin].features must enable pyo3/extension-module")
    if ABI3_FEATURE not in features:
        raise RuntimeError(
            f"[tool.maturin].features must enable {ABI3_FEATURE} so one wheel "
            "per platform covers every supported interpreter"
        )

    cargo = CARGO_MANIFEST.read_text(encoding="utf-8")
    pyo3 = re.search(r"^pyo3\s*=\s*(?P<value>.+)$", cargo, flags=re.MULTILINE)
    if pyo3 is None:
        raise RuntimeError("workspace Cargo.toml must declare pyo3")
    if "abi3-py310" not in pyo3.group("value"):
        raise RuntimeError("workspace pyo3 dependency must enable abi3-py310")


def wheel_tags(archive: zipfile.ZipFile) -> list[str]:
    """Return the wheel compatibility tags from its mandatory WHEEL metadata."""
    entries = [name for name in archive.namelist() if name.endswith(".dist-info/WHEEL")]
    if len(entries) != 1:
        raise RuntimeError(f"expected one .dist-info/WHEEL entry, found {entries}")
    return [
        line.removeprefix("Tag: ").strip()
        for line in archive.read(entries[0]).decode("utf-8").splitlines()
        if line.startswith("Tag: ")
    ]


def validate_wheel(wheel: Path) -> None:
    """Require an ABI3-tagged wheel holding exactly one ABI3 native extension."""
    with zipfile.ZipFile(wheel) as archive:
        extensions = [
            name
            for name in archive.namelist()
            if name.startswith("running_process/_native")
            and name.endswith((".so", ".pyd", ".dylib"))
        ]
        tags = wheel_tags(archive)
    expected = [f"running_process/{name}" for name in ABI3_EXTENSION_NAMES]
    if len(extensions) != 1 or extensions[0] not in expected:
        raise RuntimeError(
            f"expected exactly one ABI3 native extension (one of {expected}) "
            f"in {wheel.name}, found {extensions}"
        )
    if not tags:
        raise RuntimeError(f"wheel {wheel.name} has no compatibility tags")
    abis = {tag.split("-")[1].lower() for tag in tags if tag.count("-") == 2}
    if abis != {"abi3"}:
        raise RuntimeError(
            f"wheel {wheel.name} must advertise the abi3 ABI tag so it installs "
            f"on every supported interpreter, found tags {tags}"
        )


def platform_tags_of(wheel: Path) -> set[str]:
    """Return the platform tags a wheel filename advertises."""
    stem = wheel.name.removesuffix(".whl")
    return set(stem.rsplit("-", 1)[-1].split("."))


def validate_release_coverage(wheels: list[Path]) -> None:
    """Require one wheel per platform the release cycle builds."""
    published: set[str] = set()
    for wheel in wheels:
        published |= platform_tags_of(wheel)
    missing = [tag for tag in RELEASE_PLATFORM_TAGS if tag not in published]
    if missing:
        raise RuntimeError(
            "release is missing a wheel for: "
            + ", ".join(missing)
            + f" (found {sorted(published)})"
        )


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wheel", type=Path, action="append", default=[])
    parser.add_argument(
        "--release-dir",
        type=Path,
        help="validate every wheel in this directory and require full platform coverage",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    validate_source_configuration()
    wheels = list(args.wheel)
    if args.release_dir is not None:
        wheels.extend(sorted(args.release_dir.glob("*.whl")))
    for wheel in wheels:
        validate_wheel(wheel)
        print(f"ABI3 wheel verified: {wheel.name}")
    if args.release_dir is not None:
        validate_release_coverage(wheels)
        print(f"release wheel coverage verified: {len(wheels)} wheels")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
