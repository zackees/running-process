"""Command and workflow contracts for the no-default backend identity E2E."""

from __future__ import annotations

from pathlib import Path

from ci.__main__ import STAGES
from ci.backend_identity_e2e import ipc_e2e_command, public_contract_command

ROOT = Path(__file__).resolve().parent.parent


def test_dispatcher_exposes_the_direct_identity_execution_stage() -> None:
    assert STAGES["test-backend-identity"] == "ci.backend_identity_e2e"


def test_execution_keeps_both_contracts_in_the_no_default_feature_graph() -> None:
    for command in (public_contract_command(), ipc_e2e_command()):
        assert "--no-default-features" in command
        assert command[command.index("--features") + 1] == "backend-identity"
    assert public_contract_command()[-2:] == ("--test", "backend_identity")
    assert ipc_e2e_command()[-2:] == ("--lib", "backend_identity::direct_probe_e2e_tests")


def test_preflight_executes_the_direct_identity_contract_on_each_host() -> None:
    workflow = (ROOT / ".github" / "workflows" / "ci-preflight.yml").read_text(encoding="utf-8")
    assert "Direct backend identity contract" in workflow
    assert "ci test-backend-identity" in workflow
