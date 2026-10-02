from __future__ import annotations

from ci import soldr


def test_cargo_command_falls_back_to_raw_cargo_when_soldr_absent(monkeypatch) -> None:
    """When `soldr` is not on PATH, `cargo_command` returns the raw cargo argv.
    This is the path a local checkout without soldr takes; CI runners get
    soldr from zackees/setup-soldr and take the soldr branch."""
    monkeypatch.setattr("shutil.which", lambda _name: None)
    assert soldr.cargo_command("test", "--workspace") == [
        "cargo",
        "test",
        "--workspace",
    ]


def test_cargo_command_routes_through_soldr_when_available(monkeypatch) -> None:
    """When `soldr` is on PATH (typical local dev setup), `cargo_command`
    prefixes the argv with `soldr` so the rustup-managed toolchain is
    resolved instead of whatever stale `cargo` PATH-discovers first."""
    monkeypatch.setattr(
        "shutil.which",
        lambda name: "/usr/local/bin/soldr" if name == "soldr" else None,
    )
    # The coverage lane exports the opt-out to this very test process.
    monkeypatch.delenv(soldr.DIRECT_CARGO_ENV, raising=False)
    assert soldr.cargo_command("test", "--workspace") == [
        "soldr",
        "cargo",
        "test",
        "--workspace",
    ]


def test_cargo_command_passes_through_any_subcommand(monkeypatch) -> None:
    """Regardless of the subcommand, `cargo_command` only changes the prefix."""
    monkeypatch.setattr("shutil.which", lambda _name: None)
    for subcommand in ("clippy", "fmt", "llvm-cov", "build", "check", "package"):
        assert soldr.cargo_command(subcommand, "--workspace") == [
            "cargo",
            subcommand,
            "--workspace",
        ]


def test_maturin_command_uses_python_module() -> None:
    assert soldr.maturin_command("/tmp/python", "build", "--release") == [
        "/tmp/python",
        "-m",
        "maturin",
        "build",
        "--release",
    ]


def test_cargo_command_runs_cargo_directly_when_opted_out(monkeypatch) -> None:
    """Coverage opts out: cargo-llvm-cov must own RUSTC_WRAPPER, so its builds
    cannot go through `soldr cargo` even with soldr installed."""
    monkeypatch.setattr(
        "shutil.which",
        lambda name: "/usr/local/bin/soldr" if name == "soldr" else None,
    )
    monkeypatch.setenv(soldr.DIRECT_CARGO_ENV, "1")
    assert soldr.cargo_command("llvm-cov", "show-env") == [
        "cargo",
        "llvm-cov",
        "show-env",
    ]
