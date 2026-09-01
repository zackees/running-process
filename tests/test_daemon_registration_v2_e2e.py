"""Command/workflow contracts for the no-default v2 registration E2E."""

from __future__ import annotations

from pathlib import Path

from ci.__main__ import STAGES
from ci.daemon_registration_v2_e2e import coexistence_command, minimal_command

ROOT = Path(__file__).resolve().parent.parent


def test_dispatcher_exposes_v2_registration_execution_stage() -> None:
    assert STAGES["test-daemon-registration-v2"] == "ci.daemon_registration_v2_e2e"


def test_execution_uses_minimal_and_coexistence_feature_graphs() -> None:
    minimal = minimal_command()
    assert "--no-default-features" in minimal
    assert minimal[minimal.index("--features") + 1] == "daemon-registration-v2"
    assert minimal[-2:] == ("--test", "daemon_registration_v2")

    coexistence = coexistence_command()
    assert coexistence[coexistence.index("--features") + 1] == (
        "daemon-registration,daemon-registration-v2"
    )


def test_preflight_executes_the_v2_registration_contract_on_each_host() -> None:
    workflow = (ROOT / ".github" / "workflows" / "ci-preflight.yml").read_text(encoding="utf-8")
    assert "Daemon registration v2 writer contract" in workflow
    assert "ci test-daemon-registration-v2" in workflow
