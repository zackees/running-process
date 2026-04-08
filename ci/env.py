from __future__ import annotations

import os


def activate() -> None:
    return None


def clean_env() -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("PYTHONUTF8", "1")
    return env
