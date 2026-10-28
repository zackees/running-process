from __future__ import annotations

import subprocess

import pytest

from ci import probe_graph_contract as graph


def test_accepts_unfeatured_probe_and_preserves_evidence() -> None:
    output = "root\n├── running-process-probe v1.0 :: \n└── other\n"
    assert graph.validate(output) == "├── running-process-probe v1.0 :: "


@pytest.mark.parametrize(
    "output",
    [
        "root only",
        "running-process-probe v1.0",
        "running-process-probe v1.0 :: embed-helper",
        "running-process-probe v1.0 :: \nrunning-process-probe-daemon v1.0 :: ",
        "running-process-probe v1.0 :: \nrunning-process-probe v1.0 :: ",
    ],
)
def test_rejects_incomplete_or_unsafe_graph(output: str) -> None:
    with pytest.raises(RuntimeError):
        graph.validate(output)


def test_target_is_forwarded_and_command_failure_is_not_hidden(monkeypatch) -> None:
    calls = []
    monkeypatch.setattr(graph, "cargo_command", lambda *args: ["soldr", "cargo", *args])
    def run(command, **_kwargs):
        calls.append(command)
        return subprocess.CompletedProcess(command, 1, "", "failed graph")
    monkeypatch.setattr(graph.subprocess, "run", run)
    with pytest.raises(RuntimeError, match="failed graph"):
        graph.check("aarch64-unknown-linux-musl")
    assert calls[0][-2:] == ["--target", "aarch64-unknown-linux-musl"]
