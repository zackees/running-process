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
