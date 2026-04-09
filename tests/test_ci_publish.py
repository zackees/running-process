from __future__ import annotations

import importlib
import subprocess


def _load_publish_module():
    return importlib.import_module("ci.publish")


def test_run_capture_uses_replacement_decoding(monkeypatch) -> None:
    module = _load_publish_module()
    calls: list[dict[str, object]] = []

    def fake_run(cmd, check=True, **kwargs):
        calls.append({"cmd": cmd, "check": check, **kwargs})
        return subprocess.CompletedProcess(
            cmd,
            0,
            stdout="bad\ufffdtext\n",
            stderr="",
        )

    monkeypatch.setattr(module.subprocess, "run", fake_run)

    result = module.run_capture(["gh", "run", "view"])

    assert result == "bad\ufffdtext"
    assert calls == [
        {
            "cmd": ["gh", "run", "view"],
            "check": True,
            "capture_output": True,
            "text": True,
            "errors": "replace",
        }
    ]


def test_run_capture_allow_failure_uses_replacement_decoding(monkeypatch) -> None:
    module = _load_publish_module()
    calls: list[dict[str, object]] = []

    def fake_run(cmd, **kwargs):
        calls.append({"cmd": cmd, **kwargs})
        return subprocess.CompletedProcess(
            cmd,
            1,
            stdout="bad\ufffdtext\n",
            stderr="",
        )

    monkeypatch.setattr(module.subprocess, "run", fake_run)

    result = module.run_capture_allow_failure(["gh", "run", "view"])

    assert result.returncode == 1
    assert result.stdout == "bad\ufffdtext\n"
    assert calls == [
        {
            "cmd": ["gh", "run", "view"],
            "capture_output": True,
            "text": True,
            "errors": "replace",
        }
    ]
