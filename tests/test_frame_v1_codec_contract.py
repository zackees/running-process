"""Unit coverage for the frame-only feature graph guard."""

from __future__ import annotations

import tomllib

from ci.__main__ import STAGES
from ci.frame_v1_codec_contract import (
    EXPECTED_FEATURE,
    FEATURE,
    compile_command,
    external_consumer_command,
    graph_failures,
    load_manifest,
    manifest_failures,
)


def test_real_manifest_has_only_the_frame_codec_dependencies() -> None:
    assert manifest_failures(load_manifest()) == []


def test_identity_or_client_feature_cannot_sneak_into_frame_codec() -> None:
    manifest = tomllib.loads(
        """
        [features]
        frame-v1-codec = ["backend-identity"]
        backend-identity = ["frame-v1-codec"]
        client = ["backend-identity"]
        """
    )
    assert manifest_failures(manifest)


def test_resolver_rejects_identity_ipc_hash_and_runtime_packages() -> None:
    failures = graph_failures("running-process v1\nblake3 v1\ninterprocess v2\ntokio v1\n")
    assert "forbidden package resolved: blake3" in failures
    assert "forbidden package resolved: interprocess" in failures
    assert "forbidden package resolved: tokio" in failures


def test_compile_contract_is_exact_no_default_feature_test_target() -> None:
    command = compile_command()
    assert command[-8:] == (
        "check",
        "-p",
        "running-process",
        "--no-default-features",
        "--features",
        FEATURE,
        "--test",
        "frame_v1_codec",
    )
    assert FEATURE == "frame-v1-codec"
    assert EXPECTED_FEATURE == {"dep:prost", "dep:running-process-protocol"}


def test_external_consumer_fixture_is_an_independent_no_default_manifest() -> None:
    command = external_consumer_command("pass")
    # Local development normally routes through soldr; environments without
    # the wrapper deliberately fall back to cargo.  The fixture contract is
    # the isolated manifest and feature selection, not the local launcher.
    assert "check" in command
    assert "--manifest-path" in command
    # `replace` normalizes the separator: the manifest path is absolute and
    # uses backslashes on Windows, where this asserted a POSIX substring and
    # failed. Nothing caught it because the Windows lane died at Lint first.
    assert "frame-v1-codec-consumer/pass/Cargo.toml" in command[-1].replace("\\", "/")


def test_dispatcher_exposes_frame_v1_guard_and_runtime_contract() -> None:
    assert STAGES["guard-frame-v1-codec"] == "ci.frame_v1_codec_contract"
    assert STAGES["test-frame-v1-codec"] == "ci.frame_v1_codec_e2e"
