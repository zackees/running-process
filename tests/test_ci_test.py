from __future__ import annotations

from ci import test as ci_test


def test_main_skips_dev_wheel_reinstall_when_running_under_cli(monkeypatch) -> None:
    called = False

    def fake_ensure_dev_wheel(*args, **kwargs):
        del args, kwargs
        nonlocal called
        called = True
        return "built"

    monkeypatch.setenv(ci_test.IN_RUNNING_PROCESS_ENV, ci_test.IN_RUNNING_PROCESS_VALUE)
    monkeypatch.setattr(ci_test, "ensure_dev_wheel", fake_ensure_dev_wheel)
    monkeypatch.setattr(
        ci_test,
        "repo_python",
        lambda: ci_test.ROOT / ".venv" / "Scripts" / "python.exe",
    )
    monkeypatch.setattr(ci_test, "load_env_helpers", lambda: (lambda: None, lambda: {}))
    monkeypatch.setattr(ci_test, "run", lambda cmd: 0)
    monkeypatch.setattr(ci_test, "run_live", lambda cmd: 0)

    result = ci_test.main()

    assert result == 0
    assert called is False
