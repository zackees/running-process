"""Resolver-backed dependency contract for the kernal-api substrate (#1147)."""

from __future__ import annotations

import re
import subprocess
import sys
import tomllib
import unittest
from collections.abc import Mapping
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "crates" / "running-process" / "Cargo.toml"
FEATURE = "kernel-substrate"
TREE_COMMAND = (
    "soldr",
    "cargo",
    "tree",
    "-p",
    "running-process",
    "--no-default-features",
    "--features",
    FEATURE,
    "--edges",
    "normal,build",
    "--prefix",
    "none",
)


def load_manifest(path: Path = MANIFEST) -> Mapping[str, object]:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def selected_feature(manifest: Mapping[str, object]) -> list[str] | None:
    features = manifest.get("features")
    if not isinstance(features, Mapping):
        return None
    members = features.get(FEATURE)
    return members if isinstance(members, list) and all(isinstance(x, str) for x in members) else None


def manifest_failures(manifest: Mapping[str, object]) -> list[str]:
    failures: list[str] = []
    if selected_feature(manifest) != ["async-process"]:
        failures.append("kernel-substrate must be exactly [\"async-process\"]")
    for section in ("dependencies", "build-dependencies", "dev-dependencies"):
        dependencies = manifest.get(section, {})
        if isinstance(dependencies, Mapping) and "kernal-api" in dependencies:
            failures.append("running-process must not depend on kernal-api")
    return failures


def package_names(tree: str) -> set[str]:
    """Read Cargo's resolved package names, never manifest aliases or comments."""
    return set(re.findall(r"^([A-Za-z0-9_-]+) v[^\s]+", tree, flags=re.MULTILINE))


def resolved_tree() -> str:
    result = subprocess.run(TREE_COMMAND, cwd=ROOT, text=True, capture_output=True, check=False)
    if result.returncode:
        raise RuntimeError(result.stderr or result.stdout)
    return result.stdout


class KernelSubstrateContractTests(unittest.TestCase):
    def test_real_manifest_declares_the_semantic_feature(self) -> None:
        self.assertEqual(manifest_failures(load_manifest()), [])

    def test_comments_dev_dependencies_and_alias_text_do_not_satisfy_the_feature(self) -> None:
        manifest = tomllib.loads(
            """
            # kernel-substrate = [\"async-process\"]
            [features]
            async-process = []
            [dev-dependencies]
            kernel-substrate = { package = \"async-process\", version = \"1\" }
            """
        )
        self.assertIn("kernel-substrate must be exactly [\"async-process\"]", manifest_failures(manifest))

    def test_resolved_package_parser_uses_package_names_not_aliases_or_comments(self) -> None:
        packages = package_names("# rusqlite v0.32\nlocal-db v0.1\nrusqlite v0.32\n")
        self.assertEqual(packages, {"local-db", "rusqlite"})


def main() -> int:
    failures = manifest_failures(load_manifest())
    if failures:
        print("kernel substrate contract failed:", *failures, sep="\n  - ", file=sys.stderr)
        return 1
    print("kernel substrate manifest contract passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
