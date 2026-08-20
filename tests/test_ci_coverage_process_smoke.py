import tempfile
import unittest
from pathlib import Path
from unittest import mock

from ci import coverage_process_smoke


def test_runpm_smoke_covers_the_operator_lifecycle(monkeypatch) -> None:
    calls: list[tuple[str, ...]] = []
    working_dirs: list[Path | None] = []

    def fake_run(_binary, *args, env, **kwargs):
        del env
        calls.append(args)
        working_dirs.append(kwargs.get("cwd"))
        return ""

    monkeypatch.setattr(coverage_process_smoke.time, "sleep", lambda _delay: None)
    monkeypatch.setattr(coverage_process_smoke, "_run", fake_run)

    coverage_process_smoke.exercise_runpm(Path("/tmp/runpm"))

    assert calls[0] == ()
    assert working_dirs[0] is not None
    assert calls[1] == ("kill",)
    assert calls[2] == ("--start-daemon",)
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
        lambda _binary, *args, env, **_kwargs: calls.append(args) or "",
    )

    coverage_process_smoke.exercise_daemon_cli(Path("/tmp/daemon"), env={})

    assert ("status",) in calls
    assert ("list", "--json") in calls
    assert ("kill-zombies", "--dry-run") in calls
    assert ("sessions", "list", "--pty") in calls
    assert any(call[:2] == ("sessions", "kill-older") for call in calls)
    assert ("sessions", "log", "missing-session") in calls


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


def test_broker_smoke_covers_admin_servicedef_and_v2_lifecycle(monkeypatch) -> None:
    calls: list[tuple[str, tuple[str, ...]]] = []
    live_calls: list[Path] = []
    monkeypatch.setattr(
        coverage_process_smoke,
        "_run",
        lambda binary, *args, env, **_kwargs: calls.append((binary.name, args)) or "",
    )
    monkeypatch.setattr(
        coverage_process_smoke,
        "exercise_live_v2_broker",
        lambda binary, *, env: live_calls.append(binary),
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
    assert ("running-process-broker-v2", ("--http-port", "invalid")) in calls
    assert live_calls == [Path("/tmp/running-process-broker-v2")]


class TestProbeCoverageSmoke(unittest.TestCase):
    def test_probe_smoke_covers_queries_validation_and_profile(self) -> None:
        calls: list[tuple[tuple[str, ...], dict[str, object]]] = []

        class FinishedDaemon:
            returncode = 0
            pid = 4242

            def poll(self):
                return None

            def terminate(self):
                return None

            def communicate(self, timeout=None):
                del timeout
                return "", ""

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def fake_run(_binary, *args, **kwargs):
                calls.append((args, kwargs))
                if "profile" in args:
                    Path(args[-1]).write_text("main 1\n", encoding="utf-8")
                if "ps" in args and "--json" in args:
                    return '[{"pid": 4242}]'
                if "crashes" in args and "--json" in args:
                    return '[{"id": 7}]'
                if "fetch" in args and "999999" not in args:
                    Path(args[-1]).write_bytes(b"crash")
                return ""

            def fake_popen(*_args, **_kwargs):
                (root / "runtime" / "rpprobed.json").write_text(
                    '{"control_socket": "coverage.sock"}', encoding="utf-8"
                )
                return FinishedDaemon()

            with (
                mock.patch.object(coverage_process_smoke, "_run", fake_run),
                mock.patch.object(
                    coverage_process_smoke.tempfile,
                    "TemporaryDirectory",
                    return_value=_TempDir(root),
                ),
                mock.patch.object(coverage_process_smoke.subprocess, "Popen", fake_popen),
                mock.patch.object(
                    coverage_process_smoke.subprocess,
                    "run",
                    return_value=mock.Mock(returncode=-6, stdout=b"", stderr=b""),
                ),
                mock.patch.object(coverage_process_smoke.time, "sleep", return_value=None),
            ):
                coverage_process_smoke.exercise_probe_cli(
                    Path("/tmp/rpprobed"), Path("/tmp/rpprobe")
                )

        profile_calls = [(args, kwargs) for args, kwargs in calls if "profile" in args]
        self.assertEqual(len(profile_calls), 1)
        profile_args, profile_kwargs = profile_calls[0]
        self.assertEqual(
            profile_args[-9:-1],
            ("profile", "--seconds", "1", "--hz", "10", "--format", "collapsed", "--out"),
        )
        self.assertNotIn("expected_codes", profile_kwargs)
        self.assertTrue(any("fetch" in args for args, _ in calls))
        self.assertTrue(any("snapshot" in args for args, _ in calls))
        self.assertTrue(any("--force" in args for args, _ in calls))


class _TempDir:
    def __init__(self, path: Path) -> None:
        self.path = path

    def __enter__(self) -> str:
        return str(self.path)

    def __exit__(self, *_args) -> None:
        return None
