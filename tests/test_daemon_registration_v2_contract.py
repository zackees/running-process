"""Regression tests for the narrow direct v2 registration feature graph."""

from __future__ import annotations

import tomllib

from ci.__main__ import STAGES
from ci.daemon_registration_v2_contract import (
    EXPECTED_FEATURE,
    FEATURE,
    clippy_command,
    compile_command,
    external_consumer_command,
    graph_failures,
    load_manifest,
    manifest_failures,
)


def test_real_manifest_has_only_v2_writer_dependencies() -> None:
    assert manifest_failures(load_manifest()) == []


def test_v1_or_client_cannot_sneak_into_v2_writer() -> None:
    manifest = tomllib.loads(
        """
        [features]
        daemon-registration-v2 = ["daemon-registration", "client"]
        client = ["daemon-registration-v2"]
        """
    )
    assert manifest_failures(manifest)


def test_resolver_rejects_ipc_hash_and_runtime_packages() -> None:
    failures = graph_failures("running-process v1\nsha2 v1\ninterprocess v2\ntokio v1\n")
    assert "forbidden package resolved: sha2" in failures
    assert "forbidden package resolved: interprocess" in failures
    assert "forbidden package resolved: tokio" in failures


def test_compile_and_clippy_contracts_target_the_minimal_v2_test() -> None:
    assert compile_command()[-8:] == (
        "check",
        "-p",
        "running-process",
        "--no-default-features",
        "--features",
        FEATURE,
        "--test",
        "daemon_registration_v2",
    )
    assert clippy_command()[-11:] == (
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
    assert FEATURE == "daemon-registration-v2"
    assert EXPECTED_FEATURE == {
        "dep:prost",
        "dep:running-process-protocol",
        "running-process-platform-internal/fs",
        "running-process-platform-internal/private-dir",
    }


def test_external_consumers_prove_direct_surface_and_client_absence() -> None:
    command = external_consumer_command("pass")
    assert "check" in command
    assert "--manifest-path" in command
    assert "daemon-registration-v2-consumer/pass/Cargo.toml" in command[-1]
    assert "daemon-registration-v2-consumer/fail-client/Cargo.toml" in external_consumer_command(
        "fail-client"
    )[-1]


def test_dispatcher_exposes_v2_registration_guard_and_runtime_contract() -> None:
    assert STAGES["guard-daemon-registration-v2"] == "ci.daemon_registration_v2_contract"
    assert STAGES["test-daemon-registration-v2"] == "ci.daemon_registration_v2_e2e"
