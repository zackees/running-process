"""Resolver and external-consumer contract for `frame-v1-codec`."""

from __future__ import annotations

import os
import re
import subprocess
import sys
from collections.abc import Mapping
from pathlib import Path

import tomllib

from ci.soldr import cargo_command

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "crates" / "running-process" / "Cargo.toml"
FEATURE = "frame-v1-codec"
EXPECTED_FEATURE = {"dep:prost", "dep:running-process-protocol"}
FORBIDDEN_FEATURES = {
    "backend-identity",
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
    "blake3",
    "clap",
    "console-api",
    "console-subscriber",
    "dirs",
    "futures-util",
    "interprocess",
    "portable-pty",
    "rusqlite",
    "serde",
    "serde_json",
    "sha2",
    "tokio",
    "tokio-util",
    "toml",
    "tracing",
    "tracing-subscriber",
}
CONSUMER_ROOT = ROOT / "crates" / "running-process" / "tests" / "frame-v1-codec-consumer"
CONSUMER_TARGET_DIR = ROOT / "target" / "frame-v1-codec-consumer-contract"


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
            "frame_v1_codec",
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


def external_consumer_command(name: str) -> tuple[str, ...]:
    return tuple(
        cargo_command(
            "check",
            "--manifest-path",
            str(CONSUMER_ROOT / name / "Cargo.toml"),
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
        failures.append(f"{FEATURE} must select exactly prost and generated protocol types")
    if FORBIDDEN_FEATURES & set(selected):
        failures.append(
            f"{FEATURE} must not compose identity, client, daemon, PTY, or runtime features"
        )

    backend_identity = features.get("backend-identity")
    if not isinstance(backend_identity, list) or FEATURE not in backend_identity:
        failures.append("backend-identity must compose frame-v1-codec")
    client = features.get("client")
    if not isinstance(client, list) or "backend-identity" not in client:
        failures.append("client must retain its backend-identity composition")
    return failures


def package_names(tree: str) -> set[str]:
    return set(re.findall(r"^([A-Za-z0-9_-]+) v[^\s]+", tree, flags=re.MULTILINE))


def graph_failures(tree: str) -> list[str]:
    return [
        f"forbidden package resolved: {package}"
        for package in sorted(package_names(tree) & FORBIDDEN_PACKAGES)
    ]


def run_external_consumer(
    name: str, *, should_succeed: bool, expected: str | None = None
) -> str | None:
    environment = os.environ.copy()
    # Keep the three sequential consumer checks in one stable ignored target.
    # Soldr's compile cache can race a short-lived temporary target while its
    # build-script executable is still being installed; a normal CI cargo
    # invocation is unaffected, but this keeps the local guard deterministic.
    environment["CARGO_TARGET_DIR"] = str(CONSUMER_TARGET_DIR)
    result = subprocess.run(
        external_consumer_command(name),
        cwd=ROOT,
        text=True,
        capture_output=True,
        check=False,
        env=environment,
    )
    output = result.stdout + result.stderr
    if (result.returncode == 0) != should_succeed:
        return output or f"external consumer {name!r} returned {result.returncode}"
    if expected is not None and expected not in output:
        return f"external consumer {name!r} did not report {expected!r}:\n{output}"
    return None


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
    if not failures:
        for name, should_succeed, expected in (
            ("pass", True, None),
            ("fail-first-party", False, "collides with a first-party"),
            ("fail-reserved", False, "must lie in the registered-consumer range"),
        ):
            if failure := run_external_consumer(
                name, should_succeed=should_succeed, expected=expected
            ):
                failures.append(failure)
    if failures:
        print("frame-v1 codec contract failed:", *failures, sep="\n  - ", file=sys.stderr)
        return 1
    print("frame-v1 codec contract passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
