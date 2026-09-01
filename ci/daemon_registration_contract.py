"""Resolver and public-consumer contract for `daemon-registration`."""

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
PLATFORM_MANIFEST = ROOT / "crates" / "running-process-platform-internal" / "Cargo.toml"
FEATURE = "daemon-registration"
EXPECTED_FEATURE = {
    "dep:prost",
    "dep:running-process-protocol",
    "dep:sha2",
    "running-process-platform-internal/fs",
    "running-process-platform-internal/private-dir",
}
FORBIDDEN_FEATURES = {
    "backend-identity",
    "client",
    "client-async",
    "daemon",
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
    "tokio",
    "tokio-util",
    "toml",
    "tracing",
    "tracing-subscriber",
}
CONSUMER_ROOT = ROOT / "crates" / "running-process" / "tests" / "daemon-registration-consumer"
CONSUMER_TARGET_DIR = ROOT / "target" / "daemon-registration-consumer-contract"


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
            "daemon_registration",
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
            "daemon_registration",
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


def external_consumer_command() -> tuple[str, ...]:
    return tuple(
        cargo_command(
            "check",
            "--manifest-path",
            str(CONSUMER_ROOT / "pass" / "Cargo.toml"),
        )
    )


def load_manifest(path: Path = MANIFEST) -> Mapping[str, object]:
    with path.open("rb") as handle:
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
        failures.append(f"{FEATURE} must select only v1 protobuf, SHA-256, fs, and private-dir")
    if FORBIDDEN_FEATURES & set(selected):
        failures.append(f"{FEATURE} must not compose IPC, identity, client, daemon, PTY, or runtime features")

    client = features.get("client")
    if not isinstance(client, list) or FEATURE not in client:
        failures.append("client must compose daemon-registration for legacy source compatibility")
    return failures


def platform_manifest_failures(manifest: Mapping[str, object]) -> list[str]:
    features = manifest.get("features")
    if not isinstance(features, Mapping):
        return ["platform Cargo.toml has no [features] table"]
    private_dir = features.get("private-dir")
    if private_dir != []:
        return ["platform private-dir must not select transport or hashing dependencies"]
    ipc = features.get("ipc")
    if not isinstance(ipc, list) or "private-dir" not in ipc:
        return ["platform ipc must compose private-dir to retain its established permission behavior"]
    return []


def package_names(tree: str) -> set[str]:
    return set(re.findall(r"^([A-Za-z0-9_-]+) v[^\s]+", tree, flags=re.MULTILINE))


def graph_failures(tree: str) -> list[str]:
    return [
        f"forbidden package resolved: {package}"
        for package in sorted(package_names(tree) & FORBIDDEN_PACKAGES)
    ]


def public_source_failures() -> list[str]:
    source = (ROOT / "crates" / "running-process" / "src" / "daemon_registration.rs").read_text(
        encoding="utf-8"
    )
    failures: list[str] = []
    for forbidden in ("pub use crate::broker", "backend_identity", "frame_v1", "platform::ipc"):
        if forbidden in source:
            failures.append(f"daemon-registration public source leaks {forbidden!r}")
    service_definition = (
        ROOT
        / "crates"
        / "running-process"
        / "src"
        / "broker"
        / "server"
        / "service_def_loader.rs"
    ).read_text(encoding="utf-8")
    if "fs::write(&path, definition.encode_to_vec())?;" not in service_definition:
        failures.append("v1 service-definition write must retain its established non-atomic fs::write")

    platform_index = (
        ROOT
        / "crates"
        / "running-process-platform-internal"
        / "src"
        / "platform.rs"
    ).read_text(encoding="utf-8")
    for required in (
        '#[cfg(feature = "ipc")]\npub mod ipc;',
        '#[cfg(feature = "pty")]\npub mod terminal;',
        '#[cfg(feature = "terminal-graphics")]\npub mod terminal_graphics;',
    ):
        if required not in platform_index:
            failures.append(f"platform capability source must remain gated: {required!r}")
    return failures


def run_external_consumer() -> str | None:
    environment = os.environ.copy()
    environment["CARGO_TARGET_DIR"] = str(CONSUMER_TARGET_DIR)
    result = subprocess.run(
        external_consumer_command(),
        cwd=ROOT,
        text=True,
        capture_output=True,
        check=False,
        env=environment,
    )
    if result.returncode:
        return result.stdout + result.stderr or "external daemon-registration consumer failed"
    return None


def main() -> int:
    failures = manifest_failures(load_manifest())
    failures.extend(platform_manifest_failures(load_manifest(PLATFORM_MANIFEST)))
    failures.extend(public_source_failures())
    if not failures:
        result = subprocess.run(tree_command(), cwd=ROOT, text=True, capture_output=True, check=False)
        if result.returncode:
            failures.append(result.stderr or result.stdout)
        else:
            failures.extend(graph_failures(result.stdout))
    if not failures:
        result = subprocess.run(compile_command(), cwd=ROOT, text=True, capture_output=True, check=False)
        if result.returncode:
            failures.append(result.stderr or result.stdout)
    if not failures:
        result = subprocess.run(clippy_command(), cwd=ROOT, text=True, capture_output=True, check=False)
        if result.returncode:
            failures.append(result.stderr or result.stdout)
    if not failures:
        if failure := run_external_consumer():
            failures.append(failure)
    if failures:
        print("daemon-registration contract failed:", *failures, sep="\n  - ", file=sys.stderr)
        return 1
    print("daemon-registration contract passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
