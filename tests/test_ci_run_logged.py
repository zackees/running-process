from __future__ import annotations

import importlib
import io


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

    module._write_console_line("📦 hello\n")

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
