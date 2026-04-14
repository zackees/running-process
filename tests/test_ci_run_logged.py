from __future__ import annotations

import importlib
import io
from pathlib import Path


def _load_run_logged_module():
    return importlib.import_module("ci.run_logged")


class _Cp1252Stdout:
    encoding = "cp1252"

    def __init__(self) -> None:
        self.buffer = io.BytesIO()

    def write(self, text: str) -> int:
        text.encode(self.encoding)
        return len(text)

    def flush(self) -> None:
        return None


def test_write_console_line_replaces_unencodable_characters(monkeypatch) -> None:
    module = _load_run_logged_module()
    fake_stdout = _Cp1252Stdout()
    monkeypatch.setattr(module.sys, "stdout", fake_stdout)

    module._write_console_line("\U0001F4E6 hello\n")

    assert fake_stdout.buffer.getvalue().decode("cp1252") == "? hello\n"


def test_stack_dump_timeout_defaults_and_clamps(monkeypatch) -> None:
    module = _load_run_logged_module()

    monkeypatch.delenv(module._STACK_DUMP_TIMEOUT_ENV, raising=False)
    assert module._stack_dump_timeout_seconds() == module._DEFAULT_STACK_DUMP_TIMEOUT_SECONDS

    monkeypatch.setenv(module._STACK_DUMP_TIMEOUT_ENV, "3")
    assert module._stack_dump_timeout_seconds() == 5.0

    monkeypatch.setenv(module._STACK_DUMP_TIMEOUT_ENV, "bad")
    assert module._stack_dump_timeout_seconds() == module._DEFAULT_STACK_DUMP_TIMEOUT_SECONDS


def test_child_env_sets_python_faulthandler(monkeypatch) -> None:
    module = _load_run_logged_module()

    monkeypatch.delenv("PYTHONFAULTHANDLER", raising=False)

    env = module._child_env()

    assert env["PYTHONFAULTHANDLER"] == "1"


def test_record_output_line_tracks_tail_and_metrics() -> None:
    module = _load_run_logged_module()
    analytics = module.RunAnalytics(
        command=["python", "-m", "pytest"],
        log_path="logs/test.log",
        started_at_utc="2026-01-01T00:00:00Z",
    )

    last_output = 0.0
    last_output = module._record_output_line(
        analytics,
        line="first line\n",
        started_at=0.0,
        last_output_at=last_output,
        now=1.25,
    )
    module._record_output_line(
        analytics,
        line="second line\n",
        started_at=0.0,
        last_output_at=last_output,
        now=2.0,
    )

    assert analytics.line_count == 2
    assert analytics.byte_count == len(b"first line\nsecond line\n")
    assert analytics.first_output_seconds == 1.25
    assert analytics.max_idle_seconds == 1.25
    assert list(analytics.tail_lines) == ["first line", "second line"]


def test_render_failure_summary_includes_tail_lines() -> None:
    module = _load_run_logged_module()

    rendered = module._render_failure_summary(
        {
            "command_pretty": "uv run --module ci.test",
            "log_path": "logs/test.log",
            "exit_code": 1,
            "duration_seconds": 12.5,
            "first_output_seconds": 0.8,
            "last_idle_seconds": 4.0,
            "max_idle_seconds": 7.5,
            "idle_dump_count": 1,
            "line_count": 10,
            "byte_count": 250,
            "tail_lines": ["traceback line", "assert 1 == 2"],
        }
    )

    assert "[run_logged] failure analytics" in rendered
    assert "exit_code: 1" in rendered
    assert "traceback line" in rendered
    assert "assert 1 == 2" in rendered


def test_write_failure_analytics_creates_json_sidecar(tmp_path: Path) -> None:
    module = _load_run_logged_module()
    log_path = tmp_path / "test.log"

    analytics_path = module._write_failure_analytics(
        log_path,
        {
            "command": ["uv", "run"],
            "command_pretty": "uv run",
            "log_path": str(log_path),
            "started_at_utc": "2026-01-01T00:00:00Z",
            "finished_at_utc": "2026-01-01T00:00:05Z",
            "duration_seconds": 5.0,
            "exit_code": 1,
            "line_count": 2,
            "byte_count": 20,
            "idle_dump_count": 0,
            "first_output_seconds": 0.1,
            "last_idle_seconds": 0.3,
            "max_idle_seconds": 0.3,
            "tail_lines": ["boom"],
        },
    )

    assert analytics_path == tmp_path / "test.log.analytics.json"
    assert '"exit_code": 1' in analytics_path.read_text(encoding="utf-8")
