"""Resolver-backed dependency contract for the kernal-api substrate (#1147)."""

from __future__ import annotations

import re
import subprocess
import sys
from collections.abc import Mapping
from pathlib import Path

import tomllib

from ci.soldr import cargo_command

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "crates" / "running-process" / "Cargo.toml"
ALLOWLIST = ROOT / "docs" / "kernel-substrate-allowlist.toml"
FEATURE = "kernel-substrate"
FORBIDDEN_FEATURES = {"client", "daemon", "pty", "conpty-sidecar", "daemon-trampoline"}
# The release matrix publishes these two Windows ABI families.  Keep target
# metadata in the reviewed graph even when the lint host is another platform.
SUPPORTED_WINDOWS_TARGETS = ("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc")


def tree_command(target: str | None = None) -> tuple[str, ...]:
    command = cargo_command(
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
    if target is not None:
        command.extend(("--target", target))
    return tuple(command)


TREE_COMMAND = tree_command()


def load_manifest(path: Path = MANIFEST) -> Mapping[str, object]:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def selected_feature(manifest: Mapping[str, object]) -> list[str] | None:
    features = manifest.get("features")
    if not isinstance(features, Mapping):
        return None
    members = features.get(FEATURE)
    return (
        members if isinstance(members, list) and all(isinstance(x, str) for x in members) else None
    )


def manifest_failures(manifest: Mapping[str, object]) -> list[str]:
    failures: list[str] = []
    if selected_feature(manifest) != ["async-process"]:
        failures.append('kernel-substrate must be exactly ["async-process"]')
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


def resolved_tree(target: str | None = None) -> str:
    result = subprocess.run(
        tree_command(target), cwd=ROOT, text=True, capture_output=True, check=False
    )
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
        failures.extend(
            f"forbidden package resolved: {name}" for name in sorted(packages & set(forbidden))
        )
    return failures


def main() -> int:
    failures = manifest_failures(load_manifest())
    if not failures:
        allowlist = load_allowlist()
        for target in (None, *SUPPORTED_WINDOWS_TARGETS):
            label = target or "host"
            try:
                failures.extend(
                    f"{label}: {failure}"
                    for failure in graph_failures(resolved_tree(target), allowlist)
                )
            except RuntimeError as error:
                failures.append(f"{label}: resolver invocation failed: {error}")
    if failures:
        print("kernel substrate contract failed:", *failures, sep="\n  - ", file=sys.stderr)
        return 1
    print("kernel substrate manifest contract passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
