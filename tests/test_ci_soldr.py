from __future__ import annotations

from ci import soldr


def test_cargo_command_routes_all_subcommands_through_soldr(monkeypatch) -> None:
    monkeypatch.setattr("shutil.which", lambda name: "/usr/local/bin/soldr")
    for subcommand in ("clippy", "fmt", "llvm-cov", "build", "check", "package"):
        assert soldr.cargo_command(subcommand, "--workspace") == [
            "soldr", "cargo", subcommand, "--workspace"
        ]


def test_maturin_command_routes_through_soldr(monkeypatch) -> None:
    monkeypatch.setattr("shutil.which", lambda name: "/usr/local/bin/soldr")
    assert soldr.maturin_command("build", "--release") == [
        "soldr", "maturin", "build", "--release"
    ]


def test_coverage_cannot_bypass_soldr(monkeypatch) -> None:
    monkeypatch.setattr("shutil.which", lambda name: "/usr/local/bin/soldr")
    monkeypatch.setenv("RUNNING_PROCESS_DIRECT_CARGO", "1")
    assert soldr.cargo_command("llvm-cov", "show-env") == [
        "soldr", "cargo", "llvm-cov", "show-env"
    ]


def test_soldr_uses_uvx_when_not_on_path(monkeypatch) -> None:
    monkeypatch.setattr("shutil.which", lambda name: None)
    assert soldr.cargo_command("test") == ["uvx", "soldr", "cargo", "test"]
    assert soldr.maturin_command("build") == ["uvx", "soldr", "maturin", "build"]


def test_nextest_probe_uses_soldr(monkeypatch) -> None:
    from ci import test as ci_test
    calls = []
    monkeypatch.setattr(ci_test, "cargo_command", lambda *args: ["soldr", "cargo", *args])
    def run(command, **_kwargs):
        calls.append(command)
        return ci_test.subprocess.CompletedProcess(command, 0, "", "")
    monkeypatch.setattr(ci_test.subprocess, "run", run)
    assert ci_test._ensure_nextest_installed()
    assert calls == [["soldr", "cargo", "nextest", "--version"]]
