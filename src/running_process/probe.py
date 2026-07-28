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
import json
import os
import sys
import threading
import traceback
from dataclasses import dataclass, field
from pathlib import Path

from running_process.dump_paths import artifact_stem, stack_dump_dir, utc_now_iso
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


@dataclass
class ThreadDump:
    """One OS thread's stacks, from both sides of the interpreter boundary.

    The two lists describe the *same* thread at the same moment, presented
    side by side rather than interleaved. Interleaving requires knowing which
    native frames belong under which Python frame, which is a later slice; the
    side-by-side view is already enough to see that a thread is blocked in
    native code and which Python call put it there.
    """

    os_tid: int
    # Raw return addresses. Symbolization happens off-process, so these are
    # integers here by design, not an oversight.
    native: list[int] = field(default_factory=list)
    # Pre-resolved ``(file, line, function, text)`` entries.
    python: list[traceback.FrameSummary] = field(default_factory=list)

    def is_mixed(self) -> bool:
        """Whether both halves were captured for this thread."""
        return bool(self.native) and bool(self.python)


def snapshot_supported() -> bool:
    """Whether native stack capture works on this platform and build."""
    native = _native_module()
    if native is None or not hasattr(native, "native_probe_snapshot_supported"):
        return False
    return bool(native.native_probe_snapshot_supported())


def snapshot() -> dict[int, ThreadDump]:
    """Capture every thread's native and interpreter stacks.

    Threads are aligned by OS thread id — never by list position, which would
    silently pair unrelated stacks whenever the two views disagree about how
    many threads exist. They routinely do: the calling thread has Python frames
    but no native ones (a thread cannot suspend itself), and interpreter-less
    threads created by native code have the reverse.

    Raises ``NotImplementedError`` where native capture is unimplemented, so an
    unsupported platform is distinguishable from a process with no threads.
    """
    native_mod = _native_module()
    if native_mod is None:
        raise ProbeUnavailableError(
            "this build of running_process._native has no probe support"
        )

    # Native capture first: it suspends siblings briefly, and doing it before
    # the interpreter walk keeps the two views as close together in time as
    # possible.
    native_frames: dict[int, list[int]] = native_mod.native_probe_snapshot()

    # Map interpreter thread ids to OS thread ids. `sys._current_frames()` is
    # keyed by the former and the native capture by the latter; they are
    # different numbers.
    os_tid_by_ident: dict[int, int] = {}
    for thread in threading.enumerate():
        ident = thread.ident
        native_id = getattr(thread, "native_id", None)
        if ident is not None and native_id is not None:
            os_tid_by_ident[ident] = native_id

    dumps: dict[int, ThreadDump] = {
        os_tid: ThreadDump(os_tid=os_tid, native=list(frames))
        for os_tid, frames in native_frames.items()
    }

    for ident, frame in sys._current_frames().items():
        os_tid = os_tid_by_ident.get(ident)
        if os_tid is None:
            # A Python thread whose OS id we cannot determine. Key it by its
            # interpreter id rather than dropping it — an unpairable stack is
            # still worth reporting, and silently discarding it would look
            # like the thread did not exist.
            os_tid = ident
        dump = dumps.get(os_tid)
        if dump is None:
            dump = ThreadDump(os_tid=os_tid)
            dumps[os_tid] = dump
        dump.python = traceback.extract_stack(frame)

    return dumps


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


def write_dump(
    *,
    reason: str = "probe",
    dump_dir: Path | None = None,
    extra_metadata: dict[str, object] | None = None,
) -> Path:
    """Write this process's mixed-mode stacks as a diagnostic artifact.

    Produces two files under the shared dump directory, named with the same
    stem convention the CLI supervisor uses so an operator finds all evidence
    in one place: ``<stem>.mixed-stacks.json`` and ``<stem>.json`` metadata.

    Returns the path to the metadata file.

    This describes the **calling** process and no other. That is not a
    limitation to be worked around: the capture suspends sibling threads of
    this process, so there is no version of it that reaches across a process
    boundary. The CLI's ``py-spy``/debugger dumps target a supervised child by
    pid and remain separate for exactly that reason — emitting this artifact
    alongside them would file the supervisor's threads under the child's
    diagnostics, which is worse than having no artifact at all.

    Raises ``NotImplementedError`` where native capture is unimplemented, and
    ``ProbeUnavailableError`` on a build without probe support.
    """
    dumps = snapshot()

    directory = stack_dump_dir(dump_dir)
    directory.mkdir(parents=True, exist_ok=True)
    pid = os.getpid()
    stem = artifact_stem(reason=reason, pid=pid)

    threads = [
        {
            "os_tid": dump.os_tid,
            # Hex, because these are addresses and every other tool that will
            # consume them (a symbolizer, a disassembler, a debugger) speaks
            # hex. Decimal would need converting at every use.
            "native": [f"0x{address:x}" for address in dump.native],
            "python": [
                {
                    "file": frame.filename,
                    "line": frame.lineno,
                    "func": frame.name,
                    "text": frame.line,
                }
                for frame in dump.python
            ],
            "is_mixed": dump.is_mixed(),
        }
        for dump in sorted(dumps.values(), key=lambda d: d.os_tid)
    ]

    stacks_path = directory / f"{stem}.mixed-stacks.json"
    stacks_path.write_text(
        json.dumps(
            {
                "pid": pid,
                "runtime": "python",
                "python_version": sys.version,
                # Stated outright so nobody reads these as another process's
                # stacks, and so the absence of symbol names is understood as
                # by-design rather than as a failure.
                "scope": "calling process only",
                "native_frames": "unsymbolized return addresses",
                "threads": threads,
            },
            indent=2,
            sort_keys=True,
        ),
        encoding="utf-8",
    )

    metadata = {
        "reason": reason,
        "pid": pid,
        "timestamp_utc": utc_now_iso(),
        "artifacts": [stacks_path.name],
        "thread_count": len(threads),
        "mixed_thread_count": sum(1 for t in threads if t["is_mixed"]),
    }
    if extra_metadata:
        metadata.update(extra_metadata)
    metadata_path = directory / f"{stem}.json"
    metadata_path.write_text(
        json.dumps(metadata, indent=2, sort_keys=True), encoding="utf-8"
    )
    return metadata_path
