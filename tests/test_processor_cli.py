import pytest

from running_process import dashboard, processor_cli


def test_processor_cli_prints_help_without_a_subcommand(capsys) -> None:
    assert processor_cli.main([]) == 0
    assert "running-processor" in capsys.readouterr().out


@pytest.mark.parametrize(
    ("arguments", "forwarded"),
    [
        (["dashboard"], []),
        (["dashboard", "--port", "9000"], ["--port", "9000"]),
        (["dashboard", "--no-browser"], ["--no-browser"]),
        (
            ["dashboard", "--port", "9001", "--no-browser"],
            ["--port", "9001", "--no-browser"],
        ),
    ],
)
def test_processor_cli_forwards_dashboard_options(
    monkeypatch: pytest.MonkeyPatch,
    arguments: list[str],
    forwarded: list[str],
) -> None:
    received: list[list[str]] = []
    monkeypatch.setattr(
        dashboard,
        "main",
        lambda argv: received.append(list(argv)) or 23,
    )

    assert processor_cli.main(arguments) == 23
    assert received == [forwarded]
