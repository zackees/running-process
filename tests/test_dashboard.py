from __future__ import annotations

import json
import subprocess
from types import SimpleNamespace

import pytest

from running_process import dashboard


def test_format_originator_splits_tool_and_pid() -> None:
    assert dashboard._format_originator("codeup:1234") == "codeup (1234)"
    assert dashboard._format_originator("plain-originator") == "plain-originator"
    assert dashboard._format_originator("") == "unknown"


def test_build_process_tree_nests_children_under_tracked_parent() -> None:
    processes = [
        {"pid": 10, "parent_pid": None, "created_at": 1.0, "registered_at": 1.0},
        {"pid": 11, "parent_pid": 10, "created_at": 2.0, "registered_at": 2.0},
        {"pid": 12, "parent_pid": 11, "created_at": 3.0, "registered_at": 3.0},
        {"pid": 99, "parent_pid": 5000, "created_at": 4.0, "registered_at": 4.0},
    ]

    tree = dashboard._build_process_tree(processes)

    assert [node["pid"] for node in tree] == [10, 99]
    assert [node["pid"] for node in tree[0]["children"]] == [11]
    assert [node["pid"] for node in tree[0]["children"][0]["children"]] == [12]


def test_dashboard_payload_enriches_processes_and_summary(monkeypatch) -> None:
    monkeypatch.setattr(
        dashboard,
        "_fetch_processes_json",
        lambda: [
            {
                "pid": 101,
                "state": 1,
                "kind": "subprocess",
                "command": "python parent.py",
                "cwd": "/repo",
                "originator": "agent:9000",
                "created_at": 1700000000.0,
                "registered_at": 1700000001.0,
            },
            {
                "pid": 102,
                "state": 1,
                "kind": "pty",
                "command": "python child.py",
                "cwd": "/repo",
                "originator": "",
                "created_at": 1700000002.0,
                "registered_at": 1700000003.0,
            },
        ],
    )
    monkeypatch.setattr(dashboard, "_fetch_parent_pids", lambda pids: {101: 9000, 102: 101})

    payload = dashboard._dashboard_payload()

    assert payload["summary"] == {"tracked": 2, "roots": 1}
    assert len(payload["processes"]) == 2
    assert payload["tree"][0]["pid"] == 101
    assert payload["tree"][0]["children"][0]["pid"] == 102
    assert payload["processes"][0]["spawned_by"] == "agent (9000)"
    assert payload["processes"][1]["spawned_by"] == "tracked pid 101"
    assert payload["processes"][0]["state_name"] == "alive"


def test_fetch_processes_handles_binary_results_and_failures(monkeypatch) -> None:
    monkeypatch.setattr(dashboard.shutil, "which", lambda _name: None)
    assert dashboard._fetch_processes_json() == []

    monkeypatch.setattr(dashboard.shutil, "which", lambda _name: "/bin/daemon")
    monkeypatch.setattr(
        dashboard.subprocess,
        "run",
        lambda *args, **kwargs: SimpleNamespace(returncode=0, stdout='[{"pid": 1}]'),
    )
    assert dashboard._fetch_processes_json() == [{"pid": 1}]

    monkeypatch.setattr(
        dashboard.subprocess,
        "run",
        lambda *args, **kwargs: SimpleNamespace(returncode=0, stdout="{}"),
    )
    assert dashboard._fetch_processes_json() == []
    monkeypatch.setattr(
        dashboard.subprocess,
        "run",
        lambda *args, **kwargs: SimpleNamespace(returncode=2, stdout="[]"),
    )
    assert dashboard._fetch_processes_json() == []

    def fail(*args, **kwargs):
        raise subprocess.TimeoutExpired("daemon", 5)

    monkeypatch.setattr(dashboard.subprocess, "run", fail)
    assert dashboard._fetch_processes_json() == []


@pytest.mark.parametrize(
    ("platform", "expected_program"),
    [("win32", "powershell"), ("linux", "ps")],
)
def test_fetch_parent_pids_parses_platform_command_output(
    monkeypatch: pytest.MonkeyPatch,
    platform: str,
    expected_program: str,
) -> None:
    commands: list[list[str]] = []
    monkeypatch.setattr(dashboard.sys, "platform", platform)
    monkeypatch.setattr(
        dashboard.subprocess,
        "run",
        lambda command, **kwargs: commands.append(command)
        or SimpleNamespace(returncode=0, stdout="10 1\ninvalid\n11 nope\n"),
    )

    assert dashboard._fetch_parent_pids([]) == {}
    assert dashboard._fetch_parent_pids([10, 11]) == {10: 1}
    assert commands[0][0] == expected_program


def test_fetch_parent_pids_and_formatting_failure_paths(monkeypatch) -> None:
    monkeypatch.setattr(
        dashboard.subprocess,
        "run",
        lambda *args, **kwargs: SimpleNamespace(returncode=1, stdout="10 1"),
    )
    assert dashboard._fetch_parent_pids([10]) == {}

    def fail(*args, **kwargs):
        raise OSError("missing ps")

    monkeypatch.setattr(dashboard.subprocess, "run", fail)
    assert dashboard._fetch_parent_pids([10]) == {}
    assert dashboard._state_name(None) == "unknown"
    assert dashboard._state_name(2) == "dead"
    assert dashboard._format_timestamp(None) == "unknown"
    assert dashboard._format_timestamp("invalid") == "unknown"
    assert dashboard._format_timestamp(0) == "1970-01-01 00:00:00Z"


def test_normalize_processes_covers_external_and_unknown_parents(monkeypatch) -> None:
    monkeypatch.setattr(dashboard, "_fetch_parent_pids", lambda _pids: {2: 99})
    processes = dashboard._normalize_processes(
        [
            {"pid": 1, "state": 99},
            {"pid": 2, "state": 3},
        ]
    )
    assert processes[0]["spawned_by"] == "unknown"
    assert processes[1]["spawned_by"] == "pid 99"
    assert processes[1]["state_name"] == "zombie"


def test_dashboard_handler_serves_html_json_and_404(monkeypatch) -> None:
    handler = object.__new__(dashboard._DashboardHandler)
    responses: list[object] = []
    handler.wfile = SimpleNamespace(write=lambda body: responses.append(body))
    handler.send_response = lambda status: responses.append(status)
    handler.send_header = lambda key, value: responses.append((key, value))
    handler.end_headers = lambda: responses.append("headers-end")
    handler.send_error = lambda status: responses.append(("error", status))
    monkeypatch.setattr(dashboard, "_dashboard_payload", lambda: {"summary": {}})

    handler.path = "/"
    handler.do_GET()
    assert 200 in responses
    assert any(
        b"running-process dashboard" in item
        for item in responses
        if isinstance(item, bytes)
    )

    responses.clear()
    handler.path = "/api/processes"
    handler.do_GET()
    assert json.loads(next(item for item in responses if isinstance(item, bytes))) == {
        "summary": {}
    }

    responses.clear()
    handler.path = "/missing"
    handler.do_GET()
    assert responses == [("error", 404)]
    assert handler.log_message("ignored") is None


@pytest.mark.parametrize("no_browser", [False, True])
def test_dashboard_main_serves_and_shuts_down(
    monkeypatch: pytest.MonkeyPatch,
    no_browser: bool,
) -> None:
    events: list[object] = []

    class FakeServer:
        def __init__(self, address, handler) -> None:
            events.append((address, handler))

        def serve_forever(self) -> None:
            raise KeyboardInterrupt

        def shutdown(self) -> None:
            events.append("shutdown")

    class FakeTimer:
        def __init__(self, delay, callback, args) -> None:
            events.append((delay, callback, args))

        def start(self) -> None:
            events.append("timer-start")

    monkeypatch.setattr(dashboard, "HTTPServer", FakeServer)
    monkeypatch.setattr(dashboard.threading, "Timer", FakeTimer)
    args = ["--port", "9876"] + (["--no-browser"] if no_browser else [])

    assert dashboard.main(args) == 0
    assert "shutdown" in events
    assert ("timer-start" in events) is not no_browser
