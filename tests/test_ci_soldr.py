from __future__ import annotations

from ci import soldr


def test_cargo_command_uses_global_soldr() -> None:
    assert soldr.cargo_command("test", "--workspace") == [
        "soldr",
        "cargo",
        "test",
        "--workspace",
    ]


def test_cargo_command_bypasses_soldr_for_unsupported_subcommands() -> None:
    for subcommand in soldr.UNSUPPORTED_CARGO_SUBCOMMANDS:
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
