from __future__ import annotations

import importlib.util
import io
from pathlib import Path


def _load_run_logged_module():
    root = Path(__file__).resolve().parents[1]
    path = root / "ci" / "run_logged.py"
    spec = importlib.util.spec_from_file_location("ci.run_logged", path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


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
