"""Resolver-backed dependency contract for the kernal-api substrate (#1147)."""

from __future__ import annotations

import re
import subprocess
import sys
import tomllib
import unittest
from collections.abc import Mapping
from pathlib import Path

from ci.soldr import cargo_command

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "crates" / "running-process" / "Cargo.toml"
ALLOWLIST = ROOT / "docs" / "kernel-substrate-allowlist.toml"
FEATURE = "kernel-substrate"
FORBIDDEN_FEATURES = {"client", "daemon", "pty", "conpty-sidecar", "daemon-trampoline"}
TREE_COMMAND = tuple(cargo_command(
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
))


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
    elif FORBIDDEN_FEATURES & set(selected_feature(manifest) or []):
        failures.append("kernel-substrate must not compose client, daemon, PTY, or binary features")
    def contains_kernal_api(value: object) -> bool:
        if not isinstance(value, Mapping):
            return False
        return "kernal-api" in value or any(contains_kernal_api(child) for child in value.values())

    if contains_kernal_api(manifest):
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


def load_allowlist() -> Mapping[str, object]:
    with ALLOWLIST.open("rb") as handle:
        return tomllib.load(handle)


def graph_failures(tree: str, allowlist: Mapping[str, object]) -> list[str]:
    packages = package_names(tree)
    approved = allowlist.get("packages")
    if not isinstance(approved, Mapping):
        return ["allowlist is missing [packages]"]
    reviewed = set(approved)
    failures = [f"unreviewed resolved package: {name}" for name in sorted(packages - reviewed)]
    forbidden = allowlist.get("forbidden", [])
    if isinstance(forbidden, list):
        failures.extend(f"forbidden package resolved: {name}" for name in sorted(packages & set(forbidden)))
    return failures


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

    def test_forbidden_and_unknown_resolved_packages_fail(self) -> None:
        allowlist = {"packages": {"running-process": "root"}, "forbidden": ["rusqlite"]}
        failures = graph_failures("running-process v1\nrusqlite v1\nunknown v1\n", allowlist)
        self.assertIn("forbidden package resolved: rusqlite", failures)
        self.assertIn("unreviewed resolved package: unknown", failures)

    def test_forbidden_feature_alias_cannot_join_the_selection(self) -> None:
        manifest = {"features": {FEATURE: ["async-process", "pty"]}}
        self.assertTrue(manifest_failures(manifest))


def main() -> int:
    failures = manifest_failures(load_manifest())
    if not failures:
        try:
            failures.extend(graph_failures(resolved_tree(), load_allowlist()))
        except RuntimeError as error:
            failures.append(f"resolver invocation failed: {error}")
    if failures:
        print("kernel substrate contract failed:", *failures, sep="\n  - ", file=sys.stderr)
        return 1
    print("kernel substrate manifest contract passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
