"""Command and workflow contracts for the no-default registration E2E."""

from __future__ import annotations

from pathlib import Path

from ci.__main__ import STAGES
from ci.daemon_registration_e2e import command

ROOT = Path(__file__).resolve().parent.parent


def test_dispatcher_exposes_the_registration_execution_stage() -> None:
    assert STAGES["test-daemon-registration"] == "ci.daemon_registration_e2e"


def test_execution_uses_the_no_default_registration_graph() -> None:
    execution = command()
    assert "--no-default-features" in execution
    assert execution[execution.index("--features") + 1] == "daemon-registration"
    assert execution[-2:] == ("--test", "daemon_registration")


def test_preflight_executes_the_registration_contract_on_each_host() -> None:
    workflow = (ROOT / ".github" / "workflows" / "ci-preflight.yml").read_text(encoding="utf-8")
    assert "Daemon registration contract" in workflow
    assert "ci test-daemon-registration" in workflow
