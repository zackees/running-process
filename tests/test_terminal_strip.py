import pytest

from running_process.pty._terminal_strip import (
    _collapse_duplicate_carriage_returns,
    _normalize_csi_sequence,
    _strip_terminal_fragments,
    _TerminalControlStripper,
)


def test_capture_stripper_handles_chunked_controls_and_cursor_home() -> None:
    stripper = _TerminalControlStripper()

    assert stripper.strip(b"") == b""
    assert stripper.strip(b"plain\x08x\x7fy") == b"plainxy"
    assert stripper.strip(b"before\x1b") == b"before"
    assert stripper.strip(b"[1;") == b""
    assert stripper.strip(b"1Hafter") == b"\rafter"
    assert stripper.strip(b"\x1b[G") == b"\r"
    assert stripper.strip(b"\x1b[12Gignored") == b"ignored"
    assert stripper.strip(b"\x1b[31mred") == b"red"


@pytest.mark.parametrize("marker", [b"P", b"X", b"^", b"_"])
def test_stripper_discards_chunked_st_terminated_strings(marker: bytes) -> None:
    stripper = _TerminalControlStripper()

    assert stripper.strip(b"left\x1b" + marker + b"payload") == b"left"
    assert stripper.strip(b"-more\x1b") == b""
    assert stripper.strip(b"\\right") == b"right"


def test_stripper_discards_osc_and_unknown_escape_sequences() -> None:
    stripper = _TerminalControlStripper()

    assert stripper.strip(b"a\x1b]0;title") == b"a"
    assert stripper.strip(b" ignored\x07b") == b"b"
    assert stripper.strip(b"c\x1b7d") == b"cd"


@pytest.mark.parametrize(
    ("sequence", "expected"),
    [
        (b"", b""),
        (b"[31m", b""),
        (b"\x1b[", b""),
        (b"\x1b[m", b"\x1b[0m"),
        (b"\x1b[31m", b"\x1b[31m"),
        (b"\x1b[?25h", b"\x1b[?25h"),
        (b"\x1b[?25l", b"\x1b[?25l"),
        (b"\x1b[6n", b""),
    ],
)
def test_echo_csi_normalization(sequence: bytes, expected: bytes) -> None:
    assert _normalize_csi_sequence(sequence, mode="echo") == expected


def test_fragment_and_duplicate_carriage_return_cleanup() -> None:
    assert _strip_terminal_fragments(b"") == b""
    assert _strip_terminal_fragments(b"a1;2;3_b") == b"ab"
    assert _strip_terminal_fragments(b"1;2_stays") == b"1;2_stays"
    assert _collapse_duplicate_carriage_returns(b"") == b""
    assert _collapse_duplicate_carriage_returns(b"a\r\r\r\nb") == b"a\r\nb"
