"""Keep Windows wheels safe for free-threaded CPython (#1142).

An ABI3 extension is named ``_native.pyd`` on Windows, allowing a free-threaded
interpreter to load an extension compiled for the regular CPython runtime. Build
interpreter-specific extensions instead, then check both the source settings and
each Windows wheel before it can be published.
"""

from __future__ import annotations

import argparse
import re
import sysconfig
import zipfile
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parent.parent
PYPROJECT = ROOT / "pyproject.toml"
CARGO_MANIFEST = ROOT / "Cargo.toml"


def validate_source_configuration() -> None:
    """Reject ABI3 in the Maturin and workspace PyO3 settings."""
    pyproject = tomllib.loads(PYPROJECT.read_text(encoding="utf-8"))
    features = pyproject["tool"]["maturin"].get("features", [])
    if "pyo3/extension-module" not in features:
        raise RuntimeError("[tool.maturin].features must enable pyo3/extension-module")
    if any("abi3" in feature.lower() for feature in features):
        raise RuntimeError("[tool.maturin].features must not enable a PyO3 ABI3 feature")

    cargo = CARGO_MANIFEST.read_text(encoding="utf-8")
    pyo3 = re.search(r"^pyo3\s*=\s*(?P<value>.+)$", cargo, flags=re.MULTILINE)
    if pyo3 is None:
        raise RuntimeError("workspace Cargo.toml must declare pyo3")
    if "abi3" in pyo3.group("value").lower():
        raise RuntimeError("workspace pyo3 dependency must not enable ABI3")


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


def validate_windows_wheel(wheel: Path, *, extension_suffix: str | None = None) -> None:
    """Require exactly one interpreter-specific native extension in ``wheel``."""
    suffix = extension_suffix or sysconfig.get_config_var("EXT_SUFFIX")
    if not suffix:
        raise RuntimeError("could not determine the current Python extension suffix")
    expected_entry = f"running_process/_native{suffix}"
    with zipfile.ZipFile(wheel) as archive:
        extensions = [
            name
            for name in archive.namelist()
            if name.startswith("running_process/_native") and name.endswith(".pyd")
        ]
        if extensions != [expected_entry]:
            raise RuntimeError(
                f"expected Windows extension {expected_entry!r} in {wheel.name}, found {extensions}"
            )
        tags = wheel_tags(archive)
    if not tags:
        raise RuntimeError(f"wheel {wheel.name} has no compatibility tags")
    if any(tag.split("-")[1].lower() == "abi3" for tag in tags if "-" in tag):
        raise RuntimeError(f"wheel {wheel.name} must not advertise the ABI3 tag: {tags}")


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wheel", type=Path, action="append", default=[])
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    validate_source_configuration()
    for wheel in args.wheel:
        validate_windows_wheel(wheel)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
