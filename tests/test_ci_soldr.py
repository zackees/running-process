from __future__ import annotations

from ci import soldr


def test_cargo_command_falls_back_without_soldr(monkeypatch) -> None:
    soldr.soldr_prefix.cache_clear()
    monkeypatch.delenv(soldr.FORCE_SOLDR_ENV, raising=False)
    monkeypatch.delenv(soldr.DISABLE_SOLDR_ENV, raising=False)
    monkeypatch.setattr(soldr.shutil, "which", lambda name: None)

    assert soldr.cargo_command("test", "--workspace") == ["cargo", "test", "--workspace"]


def test_cargo_command_prefers_soldr_when_available(monkeypatch) -> None:
    soldr.soldr_prefix.cache_clear()
    monkeypatch.delenv(soldr.FORCE_SOLDR_ENV, raising=False)
    monkeypatch.delenv(soldr.DISABLE_SOLDR_ENV, raising=False)
    monkeypatch.setattr(soldr.shutil, "which", lambda name: "/tmp/uvx" if name == "uvx" else None)

    assert soldr.cargo_command("test", "--workspace") == [
        "uvx",
        "soldr",
        "cargo",
        "test",
        "--workspace",
    ]


def test_maturin_command_uses_python_module_even_when_soldr_is_disabled(monkeypatch) -> None:
    soldr.soldr_prefix.cache_clear()
    monkeypatch.delenv(soldr.FORCE_SOLDR_ENV, raising=False)
    monkeypatch.setenv(soldr.DISABLE_SOLDR_ENV, "1")

    assert soldr.maturin_command("/tmp/python", "build", "--release") == [
        "/tmp/python",
        "-m",
        "maturin",
        "build",
        "--release",
    ]


def test_maturin_command_uses_python_module_when_soldr_is_available(monkeypatch) -> None:
    soldr.soldr_prefix.cache_clear()
    monkeypatch.delenv(soldr.FORCE_SOLDR_ENV, raising=False)
    monkeypatch.delenv(soldr.DISABLE_SOLDR_ENV, raising=False)
    monkeypatch.setattr(soldr.shutil, "which", lambda name: "/tmp/uvx" if name == "uvx" else None)

    assert soldr.maturin_command("/tmp/python", "build", "--release") == [
        "/tmp/python",
        "-m",
        "maturin",
        "build",
        "--release",
    ]


def test_cargo_command_uses_forced_prefix(monkeypatch) -> None:
    soldr.soldr_prefix.cache_clear()
    monkeypatch.setenv(soldr.FORCE_SOLDR_ENV, "C:/tools/soldr.exe")

    assert soldr.cargo_command("build", "--workspace") == [
        "C:/tools/soldr.exe",
        "cargo",
        "build",
        "--workspace",
    ]
