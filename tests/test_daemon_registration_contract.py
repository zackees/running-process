"""Regression tests for the narrow direct daemon-registration feature graph."""

from __future__ import annotations

import tomllib

from ci.__main__ import STAGES
from ci.daemon_registration_contract import (
    EXPECTED_FEATURE,
    FEATURE,
    clippy_command,
    compile_command,
    external_consumer_command,
    graph_failures,
    load_manifest,
    manifest_failures,
    platform_manifest_failures,
)


def test_real_manifest_has_only_the_registration_dependencies() -> None:
    assert manifest_failures(load_manifest()) == []


def test_identity_or_client_cannot_sneak_into_registration() -> None:
    manifest = tomllib.loads(
        """
        [features]
        daemon-registration = ["client"]
        client = ["daemon-registration"]
        """
    )
    assert manifest_failures(manifest)


def test_platform_private_dir_remains_transport_free() -> None:
    assert platform_manifest_failures(
        tomllib.loads(
            """
            [features]
            private-dir = []
            ipc = ["private-dir", "dep:interprocess"]
            """
        )
    ) == []


def test_resolver_rejects_ipc_identity_client_and_runtime_packages() -> None:
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
        "daemon_registration",
    )
    assert FEATURE == "daemon-registration"
    assert EXPECTED_FEATURE == {
        "dep:prost",
        "dep:running-process-protocol",
        "dep:sha2",
        "running-process-platform-internal/fs",
        "running-process-platform-internal/private-dir",
    }


def test_strict_clippy_contract_catches_feature_gating_dead_code() -> None:
    command = clippy_command()
    assert command[-11:] == (
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


def test_external_consumer_fixture_is_an_independent_no_default_manifest() -> None:
    command = external_consumer_command()
    assert "check" in command
    assert "--manifest-path" in command
    assert "daemon-registration-consumer/pass/Cargo.toml" in command[-1]


def test_dispatcher_exposes_registration_guard_and_runtime_contract() -> None:
    assert STAGES["guard-daemon-registration"] == "ci.daemon_registration_contract"
    assert STAGES["test-daemon-registration"] == "ci.daemon_registration_e2e"
