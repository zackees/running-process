import pytest

from running_process.command_render import list2cmdline


@pytest.mark.parametrize(
    ("command", "rendered"),
    [
        ([], ""),
        (["plain", ""], 'plain ""'),
        (["two words", "tail"], '"two words" tail'),
        (["C:\\Program Files\\tool\\"], '"C:\\Program Files\\tool\\\\"'),
        ([r'a\"b'], '"a\\\\\\"b"'),
        ([r"a\b"], '"a\\b"'),
    ],
)
def test_list2cmdline_quotes_windows_arguments(
    command: list[str], rendered: str
) -> None:
    assert list2cmdline(command) == rendered
