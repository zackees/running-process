"""Regression coverage for the managed Linux Bosn stack (#123)."""

from __future__ import annotations

import unittest
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parent.parent
BOSN_MANIFEST = ROOT / "bosn.toml"
DOCKERFILE = ROOT / "docker" / "bosn" / "Dockerfile"


class TestBosnLinuxStack(unittest.TestCase):
    def test_build_toolchain_and_memory_bounds_are_declared(self) -> None:
        """The PEP 517 native build needs clang and bounded parallelism."""
        manifest = tomllib.loads(BOSN_MANIFEST.read_text(encoding="utf-8"))
        stack = manifest["stack"]["linux"]

        self.assertEqual(
            stack["env"],
            {
                "CARGO_BUILD_JOBS": "2",
                "SOLDR_JOBS": "2",
                "UV_CONCURRENT_BUILDS": "1",
            },
        )
        self.assertIn(
            "        clang " + "\\",
            DOCKERFILE.read_text(encoding="utf-8").splitlines(),
        )

    def test_source_is_read_only_and_build_state_uses_persistent_volumes(self) -> None:
        """Compile state must not spill into the host source checkout."""
        manifest = tomllib.loads(BOSN_MANIFEST.read_text(encoding="utf-8"))
        stack = manifest["stack"]["linux"]

        self.assertEqual(
            stack["mounts"],
            {"repo": {"source": ".", "destination": "/work", "readonly": True}},
        )
        self.assertEqual(
            stack["volumes"],
            {
                "target": {"scope": "stack", "destination": "/work/target"},
                "cargo-home": {"scope": "machine", "destination": "/usr/local/cargo"},
                "rustup-home": {"scope": "machine", "destination": "/usr/local/rustup"},
                "uv-cache": {"scope": "machine", "destination": "/uv/cache"},
                "uv-venv": {"scope": "stack", "destination": "/uv/venv"},
            },
        )


if __name__ == "__main__":
    unittest.main()
