"""Daemon spawning: directory layout, trampoline linking, sidecar JSON, and spawn_daemon API."""
from __future__ import annotations

import dataclasses
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


# ---------------------------------------------------------------------------
# DaemonHandle
# ---------------------------------------------------------------------------


class DaemonOutputNotAvailableError(Exception):
    """Raised when attempting to read stdout/stderr from a daemon process."""


@dataclasses.dataclass
class DaemonHandle:
    """Handle returned by :func:`spawn_daemon`.

    The daemon is fire-and-forget — stdout/stderr are redirected to a log file,
    not piped to the caller.
    """

    pid: int
    name: str
    runtime_dir: Path
    log_path: Path | None

    def is_running(self) -> bool:
        """Return True if the daemon process is still alive."""
        if sys.platform == "win32":
            return self._is_running_win32()
        try:
            os.kill(self.pid, 0)
        except OSError:
            return False
        return True

    def _is_running_win32(self) -> bool:
        """Windows-specific liveness check using GetExitCodeProcess."""
        import ctypes

        kernel32 = ctypes.windll.kernel32  # type: ignore[attr-defined]
        process_query_limited_information = 0x1000
        still_active = 259
        handle = kernel32.OpenProcess(process_query_limited_information, False, self.pid)
        if not handle:
            return False
        try:
            exit_code = ctypes.c_ulong()
            if kernel32.GetExitCodeProcess(handle, ctypes.byref(exit_code)):
                return exit_code.value == still_active
            return False
        finally:
            kernel32.CloseHandle(handle)

    def read_stdout(self) -> str:
        """Always raises — daemon stdout is not piped to the caller."""
        msg = "Daemon stdout is not available. "
        if self.log_path and self.log_path.exists():
            msg += f"Check the log file at: {self.log_path}"
        else:
            msg += "No log file was configured."
        raise DaemonOutputNotAvailableError(msg)


# ---------------------------------------------------------------------------
# spawn_daemon
# ---------------------------------------------------------------------------

# Windows creation flags
_DETACHED_PROCESS = 0x0000_0008
_CREATE_NEW_PROCESS_GROUP = 0x0000_0200


def spawn_daemon(
    cmd: list[str] | str,
    *,
    name: str,
    cwd: str | Path | None = None,
    env: dict[str, str] | None = None,
    log_path: str | Path | None = None,
) -> DaemonHandle:
    """Spawn a daemon process via the binary trampoline.

    The daemon runs detached from the caller — stdin is /dev/null, stdout/stderr
    go to *log_path* (or are discarded if not specified).

    Parameters
    ----------
    cmd:
        The command to run (list or shell string).
    name:
        Process name — the trampoline binary is hard-linked under this name so
        the daemon appears with this name in ``ps`` / Task Manager.
    cwd:
        Working directory for the daemon (default: inherit caller).
    env:
        Explicit environment variables for the daemon.  If ``None``, the
        trampoline inherits the caller's environment.
    log_path:
        Path to a log file.  The parent opens the file in append mode before
        spawning so permission errors are reported synchronously.  If ``None``,
        stdout/stderr go to ``DEVNULL``.
    """
    # 1. Resolve command to (program, args).
    if isinstance(cmd, str):
        program = cmd
        args: list[str] = []
    else:
        program = cmd[0]
        args = list(cmd[1:])

    # 2. Prepare runtime directory.
    rd = runtime_dir(name)

    # 3. Write sidecar JSON.
    write_sidecar(name, command=program, args=args, cwd=cwd, env=env)

    # 4. Hard-link the trampoline.
    trampoline = hard_link_trampoline(name)

    # 5. Open log file if requested (parent opens for sync error reporting).
    if log_path is not None:
        log_path = Path(log_path)
        log_path.parent.mkdir(parents=True, exist_ok=True)
        log_fd = open(log_path, "a")
        stdout_target: Any = log_fd
        stderr_target: Any = log_fd
    else:
        log_fd = None
        stdout_target = subprocess.DEVNULL
        stderr_target = subprocess.DEVNULL

    # 6. Spawn the trampoline.
    try:
        kwargs: dict[str, Any] = {
            "stdin": subprocess.DEVNULL,
            "stdout": stdout_target,
            "stderr": stderr_target,
        }
        if sys.platform == "win32":
            kwargs["creationflags"] = _DETACHED_PROCESS | _CREATE_NEW_PROCESS_GROUP
        else:
            kwargs["start_new_session"] = True

        proc = subprocess.Popen([str(trampoline)], **kwargs)
    finally:
        # Close the log fd in the parent — the child inherited it.
        if log_fd is not None:
            log_fd.close()

    # 7. Write PID file.
    pid_file = rd / "daemon.pid"
    pid_file.write_text(str(proc.pid), encoding="utf-8")

    return DaemonHandle(
        pid=proc.pid,
        name=name,
        runtime_dir=rd,
        log_path=log_path,
    )
