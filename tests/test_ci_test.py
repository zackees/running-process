from __future__ import annotations

import os
from pathlib import Path

from ci import test as ci_test


def test_main_runs_pytest_through_running_process_cli(monkeypatch) -> None:
    commands: list[list[str]] = []
    fake_python = Path("/tmp/fake-venv/bin/python")

    monkeypatch.delenv(ci_test.GITHUB_ACTIONS_ENV, raising=False)
    monkeypatch.delenv(ci_test.IN_RUNNING_PROCESS_ENV, raising=False)
    monkeypatch.setattr(ci_test.sys, "executable", str(fake_python))
    monkeypatch.setattr(ci_test, "ensure_dev_wheel", lambda *args, **kwargs: "built")
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

    python = str(fake_python)
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
    monkeypatch.setattr(ci_test, "load_env_helpers", lambda: (lambda: None, lambda: {}))
    monkeypatch.setattr(ci_test, "run", lambda cmd: 0)
    monkeypatch.setattr(ci_test, "run_live", lambda cmd: 0)

    result = ci_test.main([])

    assert result == 0
    assert called is False


def test_main_skips_linux_docker_preflight_on_github_actions(monkeypatch) -> None:
    commands: list[list[str]] = []
    fake_python = Path("/tmp/fake-venv/bin/python")

    monkeypatch.setenv(ci_test.GITHUB_ACTIONS_ENV, "true")
    monkeypatch.delenv(ci_test.IN_RUNNING_PROCESS_ENV, raising=False)
    monkeypatch.setattr(ci_test.sys, "executable", str(fake_python))
    monkeypatch.setattr(ci_test, "ensure_dev_wheel", lambda *args, **kwargs: "built")
    monkeypatch.setattr(ci_test, "load_env_helpers", lambda: (lambda: None, lambda: {}))
    monkeypatch.setattr(ci_test, "run", lambda cmd: commands.append(list(cmd)) or 0)
    monkeypatch.setattr(ci_test, "run_live", lambda cmd: commands.append(list(cmd)) or 0)

    result = ci_test.main([])

    python = str(fake_python)
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


def test_main_skips_linux_docker_preflight_when_env_requests_it(monkeypatch) -> None:
    commands: list[list[str]] = []
    fake_python = Path("/tmp/fake-venv/bin/python")

    monkeypatch.setenv(ci_test.SKIP_LINUX_DOCKER_ENV, "1")
    monkeypatch.delenv(ci_test.GITHUB_ACTIONS_ENV, raising=False)
    monkeypatch.delenv(ci_test.IN_RUNNING_PROCESS_ENV, raising=False)
    monkeypatch.setattr(ci_test.sys, "executable", str(fake_python))
    monkeypatch.setattr(ci_test, "ensure_dev_wheel", lambda *args, **kwargs: "built")
    monkeypatch.setattr(ci_test, "load_env_helpers", lambda: (lambda: None, lambda: {}))
    monkeypatch.setattr(ci_test, "run", lambda cmd: commands.append(list(cmd)) or 0)
    monkeypatch.setattr(ci_test, "run_live", lambda cmd: commands.append(list(cmd)) or 0)

    result = ci_test.main([])

    python = str(fake_python)
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


def test_parse_args_converts_target_and_selector_to_pytest_k_expr(monkeypatch) -> None:
    monkeypatch.delenv("RUNNING_PROCESS_REQUIRE_NATIVE_DEBUGGER_SYMBOLS", raising=False)

    pytest_args, require_symbols, coverage = ci_test.parse_args(
        ["tests/test_pty_support.py", "timeout_does_not_arm_next_expect"]
    )

    assert pytest_args == [
        "tests/test_pty_support.py",
        "-k",
        "timeout_does_not_arm_next_expect",
    ]
    assert require_symbols is False
    assert coverage is False


def test_parse_args_preserves_explicit_pytest_flags() -> None:
    pytest_args, require_symbols, coverage = ci_test.parse_args(
        ["tests/test_pty_support.py", "-k", "timeout_does_not_arm_next_expect", "-ra"]
    )

    assert pytest_args == [
        "tests/test_pty_support.py",
        "-k",
        "timeout_does_not_arm_next_expect",
        "-ra",
    ]
    assert require_symbols is False
    assert coverage is False


def test_parse_args_tracks_no_skip_without_mutating_env(monkeypatch) -> None:
    monkeypatch.delenv("RUNNING_PROCESS_REQUIRE_NATIVE_DEBUGGER_SYMBOLS", raising=False)

    pytest_args, require_symbols, coverage = ci_test.parse_args(
        ["--no-skip", "tests/test_version.py"]
    )

    assert pytest_args == ["tests/test_version.py"]
    assert require_symbols is True
    assert coverage is False
    assert "RUNNING_PROCESS_REQUIRE_NATIVE_DEBUGGER_SYMBOLS" not in os.environ


def test_pytest_exit_is_acceptable_only_allows_no_tests_for_targeted_runs() -> None:
    assert ci_test._pytest_exit_is_acceptable(0, []) is True
    assert ci_test._pytest_exit_is_acceptable(5, []) is False
    assert ci_test._pytest_exit_is_acceptable(5, ["tests/test_pty_support.py"]) is True


def test_main_allows_targeted_live_selection_with_no_matching_tests(monkeypatch) -> None:
    fake_python = Path("/tmp/fake-venv/bin/python")

    monkeypatch.setenv(ci_test.SKIP_LINUX_DOCKER_ENV, "1")
    monkeypatch.delenv(ci_test.GITHUB_ACTIONS_ENV, raising=False)
    monkeypatch.delenv(ci_test.IN_RUNNING_PROCESS_ENV, raising=False)
    monkeypatch.setattr(ci_test.sys, "executable", str(fake_python))
    monkeypatch.setattr(ci_test, "ensure_dev_wheel", lambda *args, **kwargs: "built")
    monkeypatch.setattr(ci_test, "load_env_helpers", lambda: (lambda: None, lambda: {}))
    monkeypatch.setattr(ci_test, "run", lambda cmd: 0)
    monkeypatch.setattr(ci_test, "run_live", lambda cmd: 5)

    result = ci_test.main(["tests/test_pty_support.py"])

    assert result == 0


def test_main_builds_release_wheel_before_live_tests_when_symbols_required(monkeypatch) -> None:
    commands: list[list[str]] = []
    fake_python = Path("/tmp/fake-venv/bin/python")

    monkeypatch.delenv(ci_test.GITHUB_ACTIONS_ENV, raising=False)
    monkeypatch.delenv(ci_test.IN_RUNNING_PROCESS_ENV, raising=False)
    monkeypatch.setenv("RUNNING_PROCESS_REQUIRE_NATIVE_DEBUGGER_SYMBOLS", "0")
    monkeypatch.setattr(ci_test.sys, "executable", str(fake_python))
    monkeypatch.setattr(ci_test.sys, "platform", "win32")
    monkeypatch.setattr(ci_test, "ensure_dev_wheel", lambda *args, **kwargs: "built")
    monkeypatch.setattr(ci_test, "load_env_helpers", lambda: (lambda: None, lambda: {}))
    monkeypatch.setattr(ci_test, "run", lambda cmd: commands.append(list(cmd)) or 0)
    monkeypatch.setattr(ci_test, "run_live", lambda cmd: commands.append(list(cmd)) or 0)

    result = ci_test.main(["--no-skip"])

    python = str(fake_python)
    timeout = str(ci_test.DEFAULT_COMMAND_TIMEOUT_SECONDS)
    linux_timeout = str(ci_test.DEFAULT_LINUX_TEST_TIMEOUT_SECONDS)
    release_timeout = str(ci_test.DEFAULT_RELEASE_BUILD_TIMEOUT_SECONDS)
    assert result == 0
    assert os.environ["RUNNING_PROCESS_REQUIRE_NATIVE_DEBUGGER_SYMBOLS"] == "1"
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
            release_timeout,
            "--",
            python,
            "build.py",
            "--release",
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
