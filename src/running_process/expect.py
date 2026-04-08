from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from re import Pattern
from typing import Any

ExpectPattern = str | Pattern[str]
ExpectAction = str | bytes | Callable[["ExpectMatch", Any], None] | None


@dataclass
class ExpectMatch:
    buffer: str
    matched: str
    span: tuple[int, int]
    groups: tuple[str, ...]


@dataclass
class ExpectRule:
    pattern: ExpectPattern
    action: ExpectAction = None


def search_expect_pattern(buffer: str, pattern: ExpectPattern) -> ExpectMatch | None:
    if isinstance(pattern, str):
        index = buffer.find(pattern)
        if index == -1:
            return None
        return ExpectMatch(
            buffer=buffer,
            matched=pattern,
            span=(index, index + len(pattern)),
            groups=(),
        )

    match = pattern.search(buffer)
    if match is None:
        return None
    return ExpectMatch(
        buffer=buffer,
        matched=match.group(0),
        span=match.span(),
        groups=match.groups(),
    )


def apply_expect_action(process: Any, action: ExpectAction, match: ExpectMatch) -> None:
    if action is None:
        return
    if callable(action):
        action(match, process)
        return
    if isinstance(action, bytes):
        process.write(action)
        return
    if action == "terminate":
        process.terminate()
        return
    if action == "kill":
        process.kill()
        return
    if action == "interrupt":
        if hasattr(process, "send_interrupt"):
            process.send_interrupt()
            return
        process.terminate()
        return
    process.write(action)


def ensure_text(value: str | bytes, encoding: str = "utf-8", errors: str = "replace") -> str:
    if isinstance(value, str):
        return value
    return value.decode(encoding, errors)
