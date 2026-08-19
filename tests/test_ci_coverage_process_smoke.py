from pathlib import Path

from ci import coverage_process_smoke


def test_runpm_smoke_covers_the_operator_lifecycle(monkeypatch) -> None:
    calls: list[tuple[str, ...]] = []

    monkeypatch.setattr(coverage_process_smoke.time, "sleep", lambda _delay: None)
    monkeypatch.setattr(
        coverage_process_smoke,
        "_run",
        lambda _binary, *args, env: calls.append(args) or "",
    )

    coverage_process_smoke.exercise_runpm(Path("/tmp/runpm"))

    assert calls[0] == ("kill",)
    assert calls[1] == ("--start-daemon",)
    assert ("list", "--json") in calls
    assert ("show", "coverage-smoke") in calls
    assert ("restart", "coverage-smoke") in calls
    assert ("save",) in calls
    assert ("resurrect",) in calls
    assert any(call[:2] == ("maintenance", "release-handles") for call in calls)
    assert calls[-1] == ("kill",)


def test_daemon_cli_smoke_covers_inspection_and_session_commands(monkeypatch) -> None:
    calls: list[tuple[str, ...]] = []
    monkeypatch.setattr(
        coverage_process_smoke,
        "_run",
        lambda _binary, *args, env: calls.append(args) or "",
    )

    coverage_process_smoke.exercise_daemon_cli(Path("/tmp/daemon"), env={})

    assert ("status",) in calls
    assert ("list", "--json") in calls
    assert ("kill-zombies", "--dry-run") in calls
    assert ("sessions", "list", "--pty") in calls
    assert any(call[:2] == ("sessions", "kill-older") for call in calls)


def test_cleanup_smoke_is_dry_run_and_isolated(monkeypatch) -> None:
    calls: list[tuple[str, ...]] = []
    monkeypatch.setattr(
        coverage_process_smoke,
        "_run",
        lambda _binary, *args, env: calls.append(args) or "",
    )

    coverage_process_smoke.exercise_cleanup(Path("/tmp/cleanup"))

    assert any("list" in call for call in calls)
    assert any("prune" in call for call in calls)
    assert any("uninstall" in call for call in calls)
    assert all("--confirm" not in call for call in calls)


def test_broker_smoke_covers_admin_servicedef_and_v2_no_bind(monkeypatch) -> None:
    calls: list[tuple[str, tuple[str, ...]]] = []
    monkeypatch.setattr(
        coverage_process_smoke,
        "_run",
        lambda binary, *args, env: calls.append((binary.name, args)) or "",
    )

    coverage_process_smoke.exercise_brokers(
        Path("/tmp/running-process-broker-v1"),
        Path("/tmp/running-process-broker-v2"),
    )

    assert ("running-process-broker-v1", ("status", "--json")) in calls
    assert ("running-process-broker-v1", ("metrics",)) in calls
    assert any(args[:2] == ("servicedef", "install") for _, args in calls)
    assert (
        "running-process-broker-v2",
        ("--no-bind", "--program", "coverage-broker"),
    ) in calls
