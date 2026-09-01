"""Regression tests for the narrow direct backend-identity feature graph."""

from __future__ import annotations

import tomllib

from ci.__main__ import STAGES
from ci.backend_identity_contract import (
    EXPECTED_FEATURE,
    FEATURE,
    compile_command,
    graph_failures,
    load_manifest,
    manifest_failures,
)


def test_real_manifest_has_only_the_direct_identity_dependencies() -> None:
    assert manifest_failures(load_manifest()) == []


def test_client_or_runtime_feature_cannot_sneak_into_direct_identity() -> None:
    manifest = tomllib.loads(
        """
        [features]
        backend-identity = ["client"]
        client = ["backend-identity"]
        """
    )
    assert manifest_failures(manifest)


def test_resolver_rejects_meaningful_heavyweight_client_packages() -> None:
    failures = graph_failures("running-process v1\ntokio v1\nrusqlite v1\n")
    assert "forbidden package resolved: tokio" in failures
    assert "forbidden package resolved: rusqlite" in failures


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
        "backend_identity",
    )
    assert FEATURE == "backend-identity"
    assert "client" not in EXPECTED_FEATURE


def test_dispatcher_exposes_the_backend_identity_guard() -> None:
    assert STAGES["guard-backend-identity"] == "ci.backend_identity_contract"
