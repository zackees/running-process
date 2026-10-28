"""The interactive Docker runner uses the managed Rust toolchain."""

from pathlib import Path

from ci import dev_docker


def test_cargo_command_routes_through_soldr(monkeypatch):
    calls = []
    monkeypatch.setattr(dev_docker, "ensure_image_built", lambda: 0)
    monkeypatch.setattr(dev_docker, "docker_run", lambda args, **kwargs: calls.append(args) or 0)
    assert dev_docker.cmd_cargo(["build", "-p", "running-process"]) == 0
    assert calls == [["soldr", "cargo", "build", "-p", "running-process"]]


def test_image_installs_soldr_outside_mounted_volumes():
    text = (Path(__file__).resolve().parents[1] / "docker/dev/Dockerfile").read_text()
    assert "soldr is INTENTIONALLY NOT installed" not in text
    assert "soldr" in text
    assert "UV_TOOL_BIN_DIR=/usr/local/bin" in text


def test_managed_image_tag_does_not_reuse_legacy_image():
    assert dev_docker.IMAGE != "running-process-dev:latest"


def test_managed_cache_is_mounted_for_container_runs(monkeypatch):
    calls = []

    class Result:
        returncode = 0

    monkeypatch.setattr(
        dev_docker.subprocess, "run", lambda args, **kwargs: calls.append(args) or Result()
    )
    assert dev_docker.docker_run(["soldr", "cargo", "--version"]) == 0
    assert f"{dev_docker.VOL_SOLDR}:/root/.soldr" in calls[0]
