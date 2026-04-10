from __future__ import annotations

import faulthandler
import os
import sys
import threading
from collections.abc import Iterator

import pytest

_DEFAULT_TEST_TIMEOUT_SECONDS = 20.0
_IN_RUNNING_PROCESS_ENV = "IN_RUNNING_PROCESS"
_EXPECTED_IN_RUNNING_PROCESS = "running-process-cli"


def pytest_sessionstart(session: pytest.Session) -> None:
    del session
    actual = os.environ.get(_IN_RUNNING_PROCESS_ENV)
    if actual != _EXPECTED_IN_RUNNING_PROCESS:
        raise RuntimeError(
            "pytest must run under the running-process test entrypoint; "
            f"expected {_IN_RUNNING_PROCESS_ENV}={_EXPECTED_IN_RUNNING_PROCESS!r}, "
            f"got {actual!r}. Run pytest through the running-process CLI so it sets "
            "IN_RUNNING_PROCESS=running-process-cli and can auto-dump stacks when a "
            "test hangs. Do not manually inject IN_RUNNING_PROCESS in the agent or "
            "test command."
        )


def _crash_current_process_for_test_timeout(nodeid: str, timeout_seconds: float) -> None:
    message = f"\nCRASHED!!! {nodeid} exceeded {timeout_seconds:.0f}s\n"
    os.write(2, message.encode("utf-8", errors="replace"))
    try:
        from running_process._native import native_dump_rust_debug_traces

        rust_dump = native_dump_rust_debug_traces()
        if rust_dump.strip():
            os.write(2, b"\n[running-process rust debug trace]\n")
            os.write(2, rust_dump.encode("utf-8", errors="replace"))
    except Exception:
        pass
    crash_stream = sys.__stderr__
    crash_stream.flush()
    faulthandler.dump_traceback(file=crash_stream, all_threads=True)
    crash_stream.flush()
    os._exit(1)


@pytest.fixture(autouse=True)
def _per_test_process_watchdog(request: pytest.FixtureRequest) -> Iterator[None]:
    configured_timeout = os.environ.get("RUNNING_PROCESS_TEST_TIMEOUT_SECONDS")
    timeout_seconds = (
        float(configured_timeout) if configured_timeout else _DEFAULT_TEST_TIMEOUT_SECONDS
    )
    timer = threading.Timer(
        timeout_seconds,
        _crash_current_process_for_test_timeout,
        args=(request.node.nodeid, timeout_seconds),
    )
    timer.daemon = True
    timer.start()
    try:
        yield
    finally:
        timer.cancel()


def pytest_collection_modifyitems(config: pytest.Config, items: list[pytest.Item]) -> None:
    if os.environ.get("RUNNING_PROCESS_LIVE_TESTS") == "1":
        return

    skip_live = pytest.mark.skip(
        reason="live tests require RUNNING_PROCESS_LIVE_TESTS=1"
    )
    for item in items:
        if "live" in item.keywords:
            item.add_marker(skip_live)
