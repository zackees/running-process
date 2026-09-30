"""Guard that every licence surface names the licence the `LICENSE` file grants.

The `LICENSE` file is the only actual grant. Package metadata that names a
different SPDX identity gives SBOM and compliance tooling a conflicting answer
depending on which surface it reads (#1197).
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parents[1]

# First line of the licence text -> SPDX identifier.
_HEADERS = {"MIT License": "MIT"}


def _load_toml(relative: str) -> dict:
    return tomllib.loads((ROOT / relative).read_text(encoding="utf-8"))


def _licence_file_spdx() -> str:
    header = (ROOT / "LICENSE").read_text(encoding="utf-8").splitlines()[0].strip()
    return _HEADERS[header]


class LicenseMetadataTest(unittest.TestCase):
    def test_license_file_is_recognised(self) -> None:
        self.assertEqual(_licence_file_spdx(), "MIT")

    def test_python_package_metadata_matches_license_file(self) -> None:
        license_field = _load_toml("pyproject.toml")["project"]["license"]
        self.assertEqual(license_field["text"], _licence_file_spdx())

    def test_workspace_cargo_metadata_matches_license_file(self) -> None:
        workspace = _load_toml("Cargo.toml")["workspace"]["package"]
        self.assertEqual(workspace["license"], _licence_file_spdx())

    def test_every_crate_inherits_or_matches_license_file(self) -> None:
        expected = _licence_file_spdx()
        for manifest in sorted((ROOT / "crates").glob("*/Cargo.toml")):
            package = tomllib.loads(manifest.read_text(encoding="utf-8"))["package"]
            license_value = package.get("license")
            if license_value is None:
                if package.get("publish") is False:
                    continue  # never distributed, so no metadata to conflict
                self.fail(f"{manifest.relative_to(ROOT)} declares no licence")
            if isinstance(license_value, dict):
                self.assertTrue(
                    license_value.get("workspace"),
                    f"{manifest.relative_to(ROOT)} must inherit the workspace licence",
                )
            else:
                self.assertEqual(
                    license_value, expected, str(manifest.relative_to(ROOT))
                )

    def test_no_stale_bsd_identity_in_manifests(self) -> None:
        for relative in ("pyproject.toml", "Cargo.toml"):
            text = (ROOT / relative).read_text(encoding="utf-8")
            self.assertIsNone(re.search(r"BSD[- ]3", text), relative)


if __name__ == "__main__":
    unittest.main()
