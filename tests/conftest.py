from __future__ import annotations

import os
from pathlib import Path

import pytest


def pytest_collection_modifyitems(config: pytest.Config, items: list[pytest.Item]) -> None:
    if os.environ.get("RUNNING_PROCESS_LIVE_TESTS") == "1":
        return

    skip_live = pytest.mark.skip(
        reason="live tests require RUNNING_PROCESS_LIVE_TESTS=1"
    )
    for item in items:
        if "live" in item.keywords:
            item.add_marker(skip_live)


@pytest.fixture(autouse=True)
def isolated_pid_db(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("RUNNING_PROCESS_PID_DB", str(tmp_path / "tracked-pids.sqlite3"))
