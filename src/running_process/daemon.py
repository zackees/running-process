"""Daemon spawning helpers: directory layout, trampoline linking, sidecar JSON."""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any


def _app_root() -> Path:
    """Return the platform-appropriate root for running-process app data.

    - Windows: %LOCALAPPDATA%/running-process
    - macOS:   ~/Library/Application Support/running-process
    - Linux:   ~/.local/share/running-process
    """
    if sys.platform == "win32":
        base = Path(os.environ.get("LOCALAPPDATA", Path.home() / "AppData" / "Local"))
    elif sys.platform == "darwin":
        base = Path.home() / "Library" / "Application Support"
    else:
        base = Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local" / "share"))
    return base / "running-process"


def assets_dir() -> Path:
    """Return the assets directory, creating it if needed."""
    d = _app_root() / "assets" / "trampoline"
    d.mkdir(parents=True, exist_ok=True)
    return d


def runtime_dir(name: str | None = None) -> Path:
    """Return the runtime directory, optionally for a specific daemon.

    If *name* is given, returns ``runtime/<name>/`` and creates it.
    """
    d = _app_root() / "runtime"
    if name is not None:
        d = d / name
    d.mkdir(parents=True, exist_ok=True)
    return d


def _bundled_trampoline_path() -> Path:
    """Return the path to the trampoline binary bundled inside the installed package."""
    assets = Path(__file__).resolve().parent / "assets"
    if sys.platform == "win32":
        return assets / "daemon-trampoline.exe"
    return assets / "daemon-trampoline"


def trampoline_source_path() -> Path:
    """Return the path to the trampoline binary in the app assets dir.

    On first call, copies from the bundled package assets to the app-level
    assets directory so that hard links from runtime/ always target a stable
    location on the same filesystem as ~/.running-process/.
    """
    ext = ".exe" if sys.platform == "win32" else ""
    dest = assets_dir() / f"daemon-trampoline{ext}"

    bundled = _bundled_trampoline_path()
    if not bundled.exists():
        raise FileNotFoundError(
            f"Bundled trampoline binary not found at {bundled}. "
            "Was the package built with trampoline support?"
        )

    # Copy if missing or outdated (different size).
    if not dest.exists() or dest.stat().st_size != bundled.stat().st_size:
        shutil.copy2(bundled, dest)
        # macOS: re-sign after copy to keep Gatekeeper happy.
        if sys.platform == "darwin":
            subprocess.run(
                ["codesign", "--force", "--sign", "-", str(dest)],
                check=False,
                capture_output=True,
            )

    return dest


def hard_link_trampoline(name: str) -> Path:
    """Hard-link the trampoline binary into the daemon's runtime directory.

    The link is named ``<name>`` (or ``<name>.exe`` on Windows) so the process
    shows up under that name in Task Manager / ps.

    Falls back to copy if hard-linking fails (e.g., cross-filesystem).
    On macOS copy fallback, re-signs the binary.

    Returns the path to the linked/copied trampoline.
    """
    ext = ".exe" if sys.platform == "win32" else ""
    dest_dir = runtime_dir(name)
    dest = dest_dir / f"{name}{ext}"

    if dest.exists():
        dest.unlink()

    source = trampoline_source_path()

    try:
        os.link(source, dest)
    except OSError:
        shutil.copy2(source, dest)
        if sys.platform == "darwin":
            subprocess.run(
                ["codesign", "--force", "--sign", "-", str(dest)],
                check=False,
                capture_output=True,
            )

    return dest


def write_sidecar(
    name: str,
    *,
    command: str,
    args: list[str] | None = None,
    cwd: str | Path | None = None,
    env: dict[str, str] | None = None,
) -> Path:
    """Write the daemon sidecar JSON file next to the trampoline link.

    Returns the path to the written sidecar file.
    """
    dest_dir = runtime_dir(name)
    sidecar_path = dest_dir / f"{name}.daemon.json"

    data: dict[str, Any] = {"command": str(command)}
    if args:
        data["args"] = args
    if cwd is not None:
        data["cwd"] = str(cwd)
    if env is not None:
        data["env"] = env

    sidecar_path.write_text(json.dumps(data, indent=2), encoding="utf-8")
    return sidecar_path


def cleanup_runtime(name: str) -> None:
    """Remove a daemon's runtime directory and all its contents."""
    d = _app_root() / "runtime" / name
    if d.exists():
        shutil.rmtree(d, ignore_errors=True)
