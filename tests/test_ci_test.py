from __future__ import annotations

from ci import test as ci_test


def test_main_runs_pytest_through_running_process_cli(monkeypatch) -> None:
    commands: list[list[str]] = []

    monkeypatch.delenv(ci_test.IN_RUNNING_PROCESS_ENV, raising=False)
    monkeypatch.setattr(ci_test, "ensure_dev_wheel", lambda *args, **kwargs: "built")
    monkeypatch.setattr(
        ci_test,
        "repo_python",
        lambda: ci_test.ROOT / ".venv" / "Scripts" / "python.exe",
    )
    monkeypatch.setattr(ci_test, "load_env_helpers", lambda: (lambda: None, lambda: {}))
    monkeypatch.setattr(
        ci_test,
        "run",
        lambda cmd: commands.append(list(cmd)) or 0,
    )
    monkeypatch.setattr(
        ci_test,
        "run_live",
        lambda cmd: commands.append(list(cmd)) or 0,
    )

    result = ci_test.main([])

    python = str(ci_test.ROOT / ".venv" / "Scripts" / "python.exe")
    timeout = str(ci_test.DEFAULT_COMMAND_TIMEOUT_SECONDS)
    linux_timeout = str(ci_test.DEFAULT_LINUX_TEST_TIMEOUT_SECONDS)
    assert result == 0
    assert commands == [
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            "cargo",
            "test",
            "--workspace",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "pytest",
            "-m",
            "not live",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            linux_timeout,
            "--",
            python,
            "-m",
            "ci.linux_docker",
            "all",
            "--output-dir",
            str(ci_test.ROOT / "linux"),
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "pytest",
            "-m",
            "live",
        ],
    ]


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

    result = ci_test.main([])

    assert result == 0
    assert called is False


def test_main_skips_linux_docker_preflight_on_github_actions(monkeypatch) -> None:
    commands: list[list[str]] = []

    monkeypatch.setenv(ci_test.GITHUB_ACTIONS_ENV, "true")
    monkeypatch.delenv(ci_test.IN_RUNNING_PROCESS_ENV, raising=False)
    monkeypatch.setattr(ci_test, "ensure_dev_wheel", lambda *args, **kwargs: "built")
    monkeypatch.setattr(
        ci_test,
        "repo_python",
        lambda: ci_test.ROOT / ".venv" / "Scripts" / "python.exe",
    )
    monkeypatch.setattr(ci_test, "load_env_helpers", lambda: (lambda: None, lambda: {}))
    monkeypatch.setattr(ci_test, "run", lambda cmd: commands.append(list(cmd)) or 0)
    monkeypatch.setattr(ci_test, "run_live", lambda cmd: commands.append(list(cmd)) or 0)

    result = ci_test.main([])

    python = str(ci_test.ROOT / ".venv" / "Scripts" / "python.exe")
    timeout = str(ci_test.DEFAULT_COMMAND_TIMEOUT_SECONDS)
    assert result == 0
    assert commands == [
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            "cargo",
            "test",
            "--workspace",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "pytest",
            "-m",
            "not live",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "pytest",
            "-m",
            "live",
        ],
    ]
