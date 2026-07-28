"""Probe enrollment for Python processes (#634).

Enrolling lets the probe daemon capture stacks from this process on demand.
Registration itself is handled by the Rust worker inside ``_native`` — the same
implementation native Rust callers use — so this module is a thin, explicit
front door rather than a second copy of the protocol.

Why a Python process is not just another native one
---------------------------------------------------

A Python process *is* a native interpreter binary, so nothing about it looks
different from the outside. Its stacks, though, are mixed-mode: the frames that
matter to whoever wrote the program live in the interpreter, above the machine
frames. The daemon cannot infer that, so this module declares ``runtime=python``
at registration and the daemon records the claim.

Enrolling never blocks
----------------------

``install()`` returns without doing any I/O. Discovery, connect, register and
heartbeat all run on the Rust worker thread, so a missing or wedged daemon
cannot slow interpreter startup — an absent daemon is a normal condition that
the worker retries through, not an error.
"""

import atexit
import faulthandler
import threading
from dataclasses import dataclass, field

from running_process.interrupt_handler import handle_keyboard_interrupt


@dataclass
class ProbeConfig:
    """What this process tells the daemon about itself."""

    app_class: str
    app_name: str | None = None
    app_version: str | None = None
    instance: str | None = None
    socket_override: str | None = None
    # Environment *values* are deny-by-default: process environments routinely
    # carry credentials. Only names listed here may be disclosed.
    env_allowlist: list[str] = field(default_factory=list)
    disclose_cwd: bool = False
    # faulthandler dumps Python stacks on a fatal signal. It is the interpreter
    # half of crash reporting and costs nothing until something crashes.
    enable_faulthandler: bool = True


class ProbeUnavailableError(RuntimeError):
    """The native extension was built without probe support."""


def _native_module():
    """Return the native extension, or ``None`` if probe support is absent.

    A wheel built without the ``probe`` feature simply lacks these symbols.
    That is a degraded mode, not a broken install, so callers can choose to
    continue without enrollment.
    """
    try:
        from running_process import _native
    except ImportError:
        return None
    if not hasattr(_native, "native_probe_install"):
        return None
    return _native


def is_available() -> bool:
    """Whether this build can enroll with the probe daemon."""
    return _native_module() is not None


class ProbeGuard:
    """Handle for an enrollment. Closing it deregisters this process.

    Deregistration is best-effort by design: the daemon's real liveness signal
    is the connection closing, which happens whether or not ``close()`` runs.
    A crashed process is therefore noticed just as reliably as a clean exit.
    """

    def __init__(self, handle: int) -> None:
        self._handle: int | None = handle
        self._lock = threading.Lock()

    @property
    def handle(self) -> int | None:
        """The native handle, or ``None`` once closed."""
        return self._handle

    def is_armed(self) -> bool:
        """Whether the daemon currently holds an armed registration.

        False both before the first successful registration and while
        disconnected — enrollment succeeding does not mean a daemon answered.
        """
        with self._lock:
            handle = self._handle
        if handle is None:
            return False
        native = _native_module()
        if native is None:
            return False
        return bool(native.native_probe_is_armed(handle))

    def close(self) -> bool:
        """Deregister. Idempotent; returns whether this call did the release."""
        with self._lock:
            handle, self._handle = self._handle, None
        if handle is None:
            return False
        native = _native_module()
        if native is None:
            return False
        try:
            return bool(native.native_probe_uninstall(handle))
        except KeyboardInterrupt as e:
            handle_keyboard_interrupt(e)
            return False

    def __enter__(self) -> "ProbeGuard":
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()


def install(config: ProbeConfig, *, required: bool = False) -> ProbeGuard | None:
    """Enroll this process with the probe daemon.

    Returns a guard, or ``None`` when the build lacks probe support and
    ``required`` is false. Never blocks and never raises because a daemon is
    absent — only because enrollment itself could not be set up locally.

    Set ``required=True`` to turn a probe-less build into an error rather than
    a silent no-op.
    """
    native = _native_module()
    if native is None:
        if required:
            raise ProbeUnavailableError(
                "this build of running_process._native has no probe support"
            )
        return None

    if config.enable_faulthandler:
        # O(1) and safe at import time; arms the interpreter half of crash
        # reporting so a fatal native signal still yields Python stacks.
        faulthandler.enable(all_threads=True)

    handle = native.native_probe_install(
        config.app_class,
        config.app_name,
        config.app_version,
        config.instance,
        config.socket_override,
        list(config.env_allowlist),
        config.disclose_cwd,
    )

    guard = ProbeGuard(handle)
    # A clean exit should deregister promptly rather than waiting for the
    # connection to drop.
    atexit.register(guard.close)
    return guard
