from __future__ import annotations

import os

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
