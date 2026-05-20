from __future__ import annotations

from ci import soldr


def test_cargo_command_is_plain_cargo() -> None:
    # ci/soldr.py::cargo_command is a passthrough now that CI uses
    # dtolnay/rust-toolchain + Swatinem/rust-cache instead of
    # zackees/setup-soldr. Local devs who want soldr-wrapped builds
    # invoke `./_cargo` from the repo root, not via this helper.
    assert soldr.cargo_command("test", "--workspace") == [
        "cargo",
        "test",
        "--workspace",
    ]


def test_cargo_command_passes_through_any_subcommand() -> None:
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
