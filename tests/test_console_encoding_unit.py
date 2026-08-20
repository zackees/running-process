import locale
from types import SimpleNamespace

import pytest

from running_process import console_encoding


def test_console_encoding_priority_and_fallbacks(monkeypatch) -> None:
    assert console_encoding.detect_console_encoding("latin-1") == "latin-1"

    monkeypatch.setenv("PYTHONIOENCODING", "utf-16:replace")
    assert console_encoding.detect_console_encoding() == "utf-16"
    monkeypatch.setenv("PYTHONIOENCODING", ":replace")
    assert console_encoding.detect_console_encoding() == "utf-8"

    monkeypatch.delenv("PYTHONIOENCODING", raising=False)
    monkeypatch.setattr(console_encoding.sys, "stdout", SimpleNamespace(encoding="cp1252"))
    assert console_encoding.detect_console_encoding() == "cp1252"

    monkeypatch.setattr(console_encoding.sys, "stdout", SimpleNamespace(encoding=None))
    monkeypatch.setattr(
        console_encoding.locale, "getpreferredencoding", lambda _do_setlocale: "latin-1"
    )
    assert console_encoding.detect_console_encoding() == "latin-1"
    monkeypatch.setattr(console_encoding.locale, "getpreferredencoding", lambda _do_setlocale: "")
    assert console_encoding.detect_console_encoding() == "utf-8"

    def fail(_do_setlocale):
        raise locale.Error("unavailable")

    monkeypatch.setattr(console_encoding.locale, "getpreferredencoding", fail)
    assert console_encoding.detect_console_encoding() == "utf-8"


@pytest.mark.parametrize(
    ("text", "encoding", "expected"),
    [
        ("", "ascii", ""),
        ("plain", "ascii", "plain"),
        ("café", "ascii", "caf?"),
        ("☃", "missing-codec", "☃"),
    ],
)
def test_sanitize_for_encoding_never_raises(
    text: str,
    encoding: str,
    expected: str,
) -> None:
    assert console_encoding.sanitize_for_encoding(text, encoding) == expected
