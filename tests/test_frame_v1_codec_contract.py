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
    failures = graph_failures("running-process v1\nblake3 v1\ninterprocess v2\nmio v1\n")
    assert "forbidden package resolved: blake3" in failures
    assert "forbidden package resolved: interprocess" in failures
    assert "forbidden package resolved: mio" in failures


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


def test_consumer_target_honors_writable_root() -> None:
    from unittest.mock import patch

    from ci.frame_v1_codec_contract import consumer_target_dir

    with patch.dict("os.environ", {"CARGO_TARGET_DIR": "/writable/build"}):
        assert str(consumer_target_dir()) == "/writable/build/frame-v1-codec-consumer-contract"


def test_staged_fixture_preserves_dependency_and_source() -> None:
    import tempfile
    from pathlib import Path

    from ci.frame_v1_codec_contract import CONSUMER_ROOT, ROOT, stage_consumer

    original = (CONSUMER_ROOT / "pass" / "Cargo.toml").read_bytes()
    with tempfile.TemporaryDirectory() as scratch:
        manifest = stage_consumer("pass", Path(scratch))
        data = tomllib.loads(manifest.read_text())
        dependency = data["dependencies"]["running-process"]
        assert Path(dependency["path"]) == ROOT / "crates" / "running-process"
        assert dependency["features"] == ["frame-v1-codec"]
        assert dependency["default-features"] is False
        assert (manifest.parent / "src" / "main.rs").read_bytes() == (
            CONSUMER_ROOT / "pass" / "src" / "main.rs"
        ).read_bytes()
    assert (CONSUMER_ROOT / "pass" / "Cargo.toml").read_bytes() == original
