"""Resolver-backed contract for the opt-in direct backend identity surface."""

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
FEATURE = "backend-identity"
EXPECTED_FEATURE = {
    "frame-v1-codec",
    "running-process-platform-internal/ipc",
    "dep:blake3",
    "dep:sha2",
    "dep:getrandom",
    "dep:serde",
    "dep:serde_json",
}
FORBIDDEN_FEATURES = {
    "client",
    "client-async",
    "daemon",
    "pty",
    "conpty-sidecar",
    "daemon-trampoline",
    "process-inspection",
    "terminal-graphics",
}
FORBIDDEN_PACKAGES = {
    "clap",
    "console-api",
    "console-subscriber",
    "portable-pty",
    "rusqlite",
    "tokio",
    "tokio-util",
    "toml",
    "tracing",
    "tracing-subscriber",
}


def compile_command() -> tuple[str, ...]:
    return tuple(
        cargo_command(
            "check",
            "-p",
            "running-process",
            "--no-default-features",
            "--features",
            FEATURE,
            "--test",
            "backend_identity",
        )
    )


def tree_command() -> tuple[str, ...]:
    return tuple(
        cargo_command(
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
    )


def load_manifest() -> Mapping[str, object]:
    with MANIFEST.open("rb") as handle:
        return tomllib.load(handle)


def manifest_failures(manifest: Mapping[str, object]) -> list[str]:
    features = manifest.get("features")
    if not isinstance(features, Mapping):
        return ["Cargo.toml has no [features] table"]
    selected = features.get(FEATURE)
    if not isinstance(selected, list) or not all(isinstance(item, str) for item in selected):
        return [f"{FEATURE} must be an explicit feature list"]

    failures: list[str] = []
    if set(selected) != EXPECTED_FEATURE:
        failures.append(f"{FEATURE} must select exactly the direct identity dependencies")
    if FORBIDDEN_FEATURES & set(selected):
        failures.append(f"{FEATURE} must not compose client, daemon, PTY, or runtime features")

    client = features.get("client")
    if not isinstance(client, list) or FEATURE not in client:
        failures.append("client must compose backend-identity for source compatibility")
    return failures


def package_names(tree: str) -> set[str]:
    return set(re.findall(r"^([A-Za-z0-9_-]+) v[^\s]+", tree, flags=re.MULTILINE))


def graph_failures(tree: str) -> list[str]:
    return [
        f"forbidden package resolved: {package}"
        for package in sorted(package_names(tree) & FORBIDDEN_PACKAGES)
    ]


def main() -> int:
    failures = manifest_failures(load_manifest())
    if not failures:
        result = subprocess.run(
            tree_command(), cwd=ROOT, text=True, capture_output=True, check=False
        )
        if result.returncode:
            failures.append(result.stderr or result.stdout)
        else:
            failures.extend(graph_failures(result.stdout))
    if not failures:
        result = subprocess.run(
            compile_command(), cwd=ROOT, text=True, capture_output=True, check=False
        )
        if result.returncode:
            failures.append(result.stderr or result.stdout)
    if failures:
        print("backend identity contract failed:", *failures, sep="\n  - ", file=sys.stderr)
        return 1
    print("backend identity contract passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
