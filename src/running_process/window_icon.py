"""Set the host console/terminal window icon (#577).

Why this reports a capability instead of just doing it
------------------------------------------------------

Most terminals do not let a running program change their window icon, and they
do not say so — the underlying call succeeds and nothing happens. Windows
Terminal is the case that matters: it hosts the session in a pseudo-console
whose window handle is real but hidden, so setting an icon on it succeeds
against a window nobody can see.

A function that returned quietly there would be worse than one that failed: you
would ship a feature that silently does nothing on the default terminal of
every recent Windows install, with no signal anything was wrong.

So :func:`icon_support` reports whether it will work and why not, and
:func:`set_host_icon` raises rather than pretending.

Supported today: the classic Windows console (``conhost.exe``). Everything
else — Windows Terminal, macOS Terminal.app and iTerm2, Wayland compositors,
Alacritty, Ghostty — deliberately reserves the window decoration to the
terminal, and no in-process API changes that.
"""

from __future__ import annotations


class IconUnsupportedError(RuntimeError):
    """The host terminal will never accept an icon from this process.

    Distinct from a bad icon file: retrying, or supplying different data, will
    not help. Callers should stop asking rather than treat it as transient.
    """


def _native_module():
    """Return the native extension, or ``None`` if it lacks icon support."""
    try:
        from running_process import _native
    except ImportError:
        return None
    if not hasattr(_native, "native_window_icon_support"):
        return None
    return _native


def icon_support() -> str | None:
    """Why the host cannot accept an icon, or ``None`` when it can.

    Returning the reason rather than a bare bool is deliberate: a caller that
    only learns "no" has nothing to log, and cannot tell "this terminal never
    allows it" from "this process has no console attached right now".
    """
    native = _native_module()
    if native is None:
        return "this build of running_process._native has no window-icon support"
    return native.native_window_icon_support()


def is_supported() -> bool:
    """Whether the host window will accept an icon."""
    return icon_support() is None


def set_host_icon(path: str) -> None:
    """Set this process's host console window icon from a ``.ico`` file.

    Raises :class:`IconUnsupportedError` when the terminal cannot accept one,
    and ``OSError`` when the file itself cannot be loaded — different problems
    with different remedies.
    """
    native = _native_module()
    if native is None:
        raise IconUnsupportedError(
            "this build of running_process._native has no window-icon support"
        )
    try:
        native.native_set_window_icon_from_path(str(path))
    except RuntimeError as exc:
        # The native layer raises RuntimeError only for an unsupported host;
        # a bad file arrives as OSError and is left to propagate unchanged.
        raise IconUnsupportedError(str(exc)) from exc
