"""Source-level dependency contract for #1144.

The contract is intentionally structural: a feature branch inside the root
build script would still make Cargo compile its build dependencies.  The
protocol code generator must therefore live in a client-only package.
"""

from __future__ import annotations

import unittest
from pathlib import Path

import tomllib


ROOT = Path(__file__).resolve().parent.parent
ROOT_MANIFEST = ROOT / "crates" / "running-process" / "Cargo.toml"
ROOT_LIB = ROOT / "crates" / "running-process" / "src" / "lib.rs"
ROOT_BUILD = ROOT / "crates" / "running-process" / "build.rs"
PROTOCOL = ROOT / "crates" / "running-process-protocol"
PROTOCOL_MANIFEST = PROTOCOL / "Cargo.toml"
PROTOCOL_BUILD = PROTOCOL / "build.rs"


class TestProtocolCodegenIsolation(unittest.TestCase):
    def test_process_only_root_has_no_protocol_build_script(self) -> None:
        """RED before #1144: root build.rs compiles every broker schema."""
        self.assertFalse(ROOT_BUILD.exists())

    def test_client_owns_the_optional_protocol_package(self) -> None:
        manifest = tomllib.loads(ROOT_MANIFEST.read_text(encoding="utf-8"))

        self.assertIn("dep:running-process-protocol", manifest["features"]["client"])
        protocol = manifest["dependencies"]["running-process-protocol"]
        self.assertTrue(protocol["optional"])
        self.assertEqual(protocol["path"], "../running-process-protocol")
        self.assertNotIn("prost-build", manifest.get("build-dependencies", {}))
        self.assertNotIn("protox", manifest.get("build-dependencies", {}))

    def test_protocol_package_owns_codegen_and_root_reexports_it(self) -> None:
        protocol_manifest = tomllib.loads(PROTOCOL_MANIFEST.read_text(encoding="utf-8"))

        self.assertTrue(PROTOCOL_BUILD.is_file())
        self.assertIn("prost-build", protocol_manifest["build-dependencies"])
        self.assertIn("protox", protocol_manifest["build-dependencies"])
        self.assertIn("running_process_protocol::daemon", ROOT_LIB.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
