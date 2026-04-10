from __future__ import annotations

import importlib
import subprocess
from pathlib import Path

import pytest


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


def test_download_artifacts_scopes_each_run_to_expected_artifact(
    monkeypatch, tmp_path: Path
) -> None:
    module = _load_publish_module()
    dist_dir = tmp_path / "dist"
    monkeypatch.setattr(module, "DIST_DIR", dist_dir)

    downloaded: list[list[str]] = []

    def fake_run(cmd, **kwargs):
        downloaded.append(cmd)
        pattern = cmd[cmd.index("--pattern") + 1]
        target_dir = Path(cmd[cmd.index("--dir") + 1]) / pattern
        target_dir.mkdir(parents=True, exist_ok=True)
        if pattern == "wheels-linux-x86":
            (target_dir / "running_process-3.0.3.tar.gz").write_text("sdist", encoding="utf-8")
        (target_dir / f"running_process-3.0.3-{pattern}.whl").write_text(
            "wheel", encoding="utf-8"
        )
        return subprocess.CompletedProcess(cmd, 0)

    monkeypatch.setattr(module, "run", fake_run)

    artifacts = module.download_artifacts(
        "owner/repo",
        {workflow_file: index for index, workflow_file in enumerate(module.WORKFLOWS, start=1)},
    )

    assert len(downloaded) == len(module.WORKFLOWS)
    assert all("--pattern" in cmd for cmd in downloaded)
    assert {cmd[cmd.index("--pattern") + 1] for cmd in downloaded} == set(module.WORKFLOWS.values())
    assert {path.name for path in artifacts} == {
        "running_process-3.0.3.tar.gz",
        *{
            f"running_process-3.0.3-{artifact_name}.whl"
            for artifact_name in module.WORKFLOWS.values()
        },
    }


def test_download_artifacts_fails_when_expected_artifact_directory_missing(
    monkeypatch, tmp_path: Path
) -> None:
    module = _load_publish_module()
    monkeypatch.setattr(module, "DIST_DIR", tmp_path / "dist")

    def fake_run(cmd, **kwargs):
        return subprocess.CompletedProcess(cmd, 0)

    monkeypatch.setattr(module, "run", fake_run)

    with pytest.raises(SystemExit) as exc:
        module.download_artifacts("owner/repo", {"windows-x86.yml": 123})

    assert str(exc.value) == (
        "windows-x86.yml did not produce expected artifact directory wheels-windows-x86"
    )


def test_select_expected_artifacts_returns_only_matching_existing_files(tmp_path: Path) -> None:
    module = _load_publish_module()
    artifacts = [
        tmp_path / "running_process-3.0.3.tar.gz",
        tmp_path / "running_process-3.0.3-cp313-cp313-win_amd64.whl",
        tmp_path / "running_process-3.0.3-cp313-cp313-win_arm64.whl",
        tmp_path / "running_process-3.0.3-cp313-cp313-macosx_11_0_arm64.whl",
        tmp_path / "running_process-3.0.3-cp313-cp313-musllinux_1_2_aarch64.whl",
        tmp_path / "stray-file.whl",
    ]
    for path in artifacts:
        path.write_text("artifact", encoding="utf-8")

    selected, missing = module.select_expected_artifacts(
        artifacts, name="running_process", version="3.0.3"
    )

    assert {path.name for path in selected} == {
        "running_process-3.0.3.tar.gz",
        "running_process-3.0.3-cp313-cp313-win_amd64.whl",
        "running_process-3.0.3-cp313-cp313-win_arm64.whl",
        "running_process-3.0.3-cp313-cp313-macosx_11_0_arm64.whl",
        "running_process-3.0.3-cp313-cp313-musllinux_1_2_aarch64.whl",
    }
    assert missing == [
        "running_process-3.0.3-*linux*_x86_64.whl",
        "running_process-3.0.3-*-macosx*_x86_64.whl",
    ]


def test_select_expected_artifacts_skips_missing_files_on_disk(tmp_path: Path) -> None:
    module = _load_publish_module()
    present = tmp_path / "running_process-3.0.3.tar.gz"
    missing_file = tmp_path / "running_process-3.0.3-cp313-cp313-win_amd64.whl"
    present.write_text("artifact", encoding="utf-8")

    selected, missing = module.select_expected_artifacts(
        [present, missing_file], name="running_process", version="3.0.3"
    )

    assert [path.name for path in selected] == ["running_process-3.0.3.tar.gz"]
    assert "running_process-3.0.3-*-win_amd64.whl" in missing
