"""Guard the repo-local clud configuration in `.clud/settings.json`.

An explicit `soldr_version` pin makes clud reconcile the installed soldr on
every launch (`uv tool install --force soldr==<pin>`), which downgrades the
contributor's global soldr. The file must leave soldr unpinned: clud then uses
whatever soldr is installed and only installs the latest release when soldr is
missing.
"""

from __future__ import annotations

import json
from pathlib import Path

CLUD_SETTINGS = Path(__file__).resolve().parents[1] / ".clud" / "settings.json"


def _find_key_paths(node: object, key: str, path: str = "") -> list[str]:
    """Return the dotted path of every occurrence of `key` at any depth."""
    found: list[str] = []
    if isinstance(node, dict):
        for name, value in node.items():
            child = f"{path}.{name}" if path else name
            if name == key:
                found.append(child)
            found.extend(_find_key_paths(value, key, child))
    elif isinstance(node, list):
        for index, value in enumerate(node):
            found.extend(_find_key_paths(value, key, f"{path}[{index}]"))
    return found


def _load_settings() -> dict:
    return json.loads(CLUD_SETTINGS.read_text(encoding="utf-8"))


def test_clud_settings_does_not_pin_soldr_version() -> None:
    pins = _find_key_paths(_load_settings(), "soldr_version")
    assert pins == [], (
        f"{CLUD_SETTINGS} pins soldr at {pins}; clud force-reinstalls that exact "
        "version on every launch and downgrades the global soldr. Remove the key "
        "so clud uses the installed soldr (installing latest only when missing)."
    )


def test_clud_settings_keeps_soldr_install_and_shims_enabled() -> None:
    rust = _load_settings()["optimize"]["rust"]
    assert rust["install_soldr"] is True
    assert rust["use_soldr_shims"] is True
