"""Resolver and external-consumer contract for `daemon-registration-v2`."""

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
FEATURE = "daemon-registration-v2"
EXPECTED_FEATURE = {
    "dep:prost",
    "dep:running-process-protocol",
    "running-process-platform-internal/fs",
    "running-process-platform-internal/private-dir",
}
FORBIDDEN_FEATURES = {
    "backend-identity",
    "client",
    "client-async",
    "daemon",
    "daemon-registration",
    "pty",
    "conpty-sidecar",
    "daemon-trampoline",
    "frame-v1-codec",
    "process-inspection",
    "terminal-graphics",
}
FORBIDDEN_PACKAGES = {
    "blake3",
    "clap",
    "console-api",
    "console-subscriber",
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
CONSUMER_ROOT = ROOT / "crates" / "running-process" / "tests" / "daemon-registration-v2-consumer"
CONSUMER_TARGET_DIR = ROOT / "target" / "daemon-registration-v2-consumer-contract"


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
            "daemon_registration_v2",
        )
    )


def clippy_command() -> tuple[str, ...]:
    return tuple(
        cargo_command(
            "clippy",
            "-p",
            "running-process",
            "--no-default-features",
            "--features",
            FEATURE,
            "--test",
            "daemon_registration_v2",
            "--",
            "-D",
            "warnings",
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
        cargo_command("check", "--manifest-path", str(CONSUMER_ROOT / name / "Cargo.toml"))
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
        failures.append(
            f"{FEATURE} must select only prost, generated protocol, fs, and private-dir"
        )
    if FORBIDDEN_FEATURES & set(selected):
        failures.append(
            f"{FEATURE} must not compose v1, IPC, identity, client, daemon, "
            "PTY, or runtime features"
        )

    client = features.get("client")
    if not isinstance(client, list) or FEATURE not in client:
        failures.append(
            "client must compose daemon-registration-v2 for legacy source compatibility"
        )
    return failures


def package_names(tree: str) -> set[str]:
    return set(re.findall(r"^([A-Za-z0-9_-]+) v[^\s]+", tree, flags=re.MULTILINE))


def graph_failures(tree: str) -> list[str]:
    return [
        f"forbidden package resolved: {package}"
        for package in sorted(package_names(tree) & FORBIDDEN_PACKAGES)
    ]


def source_failures() -> list[str]:
    source = (ROOT / "crates" / "running-process" / "src" / "daemon_registration_v2.rs").read_text(
        encoding="utf-8"
    )
    failures: list[str] = []
    for forbidden in ("pub use crate::broker", "backend_identity", "frame_v1", "platform::ipc"):
        if forbidden in source:
            failures.append(f"daemon-registration-v2 public source leaks {forbidden!r}")
    if "std::fs::write(&path, definition.encode_to_vec())?;" not in source:
        failures.append(
            "v2 service-definition write must retain its established non-atomic fs::write"
        )

    compat = (
        ROOT / "crates" / "running-process" / "src" / "broker" / "protocol_v2" / "mod.rs"
    ).read_text(encoding="utf-8")
    if "pub use crate::daemon_registration_v2::{" not in compat:
        failures.append(
            "legacy protocol_v2 writer path must re-export the canonical v2 implementation"
        )
    if (ROOT / "crates" / "running-process" / "src" / "broker" / "protocol_v2" / "io.rs").exists():
        failures.append("legacy protocol_v2 must not retain a duplicate v2 writer implementation")
    return failures


def run_external_consumer(name: str, *, should_succeed: bool) -> str | None:
    environment = os.environ.copy()
    environment["CARGO_TARGET_DIR"] = str(CONSUMER_TARGET_DIR)
    result = subprocess.run(
        external_consumer_command(name),
        cwd=ROOT,
        text=True,
        capture_output=True,
        check=False,
        env=environment,
    )
    if (result.returncode == 0) != should_succeed:
        return (
            result.stdout + result.stderr
            or f"external consumer {name!r} returned {result.returncode}"
        )
    return None


def main() -> int:
    failures = manifest_failures(load_manifest())
    failures.extend(source_failures())
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
        result = subprocess.run(
            clippy_command(), cwd=ROOT, text=True, capture_output=True, check=False
        )
        if result.returncode:
            failures.append(result.stderr or result.stdout)
    if not failures:
        for name, should_succeed in (("pass", True), ("fail-client", False)):
            if failure := run_external_consumer(name, should_succeed=should_succeed):
                failures.append(failure)
    if failures:
        print("daemon-registration-v2 contract failed:", *failures, sep="\n  - ", file=sys.stderr)
        return 1
    print("daemon-registration-v2 contract passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
