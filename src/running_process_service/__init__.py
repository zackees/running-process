"""Supervised single-instance services and a client that never blocks.

A light top-level package: importing it loads running-process's native
extension and nothing else -- not the ``running_process`` package, whose
import pulls in the whole process API. That matters to the callers this is
for, such as code running inside another program's UI thread.

Service side::

    from running_process_service import ServiceSpec, serve

    spec = ServiceSpec(name="my-svc", version="1.0.0", payload_protocol=0xF0B7)
    serve(spec, lambda payload: b"pong:" + payload)    # blocks; one per name

Client side, from a loop that may only poll::

    client = ServiceClient.connect(spec.with_command([sys.executable, "svc.py"]))
    try:
        rid = client.send(b"ping")      # never waits
    except (ServiceUnavailable, QueueFull):
        pass                            # drop it; try again on a later tick
    reply = client.try_recv()           # Reply(request_id, payload, error) or None

The client's supervisor thread finds the service through its identity file,
health-probes it every ``health_interval`` seconds, and when it is absent
spawns ``command`` (detached, under a file lock, at most
``spawn_budget[0]`` times per ``spawn_budget[1]`` seconds). See
``docs/SERVICE.md``.
"""

from __future__ import annotations

import dataclasses
import importlib.machinery
import importlib.util
import os
import sys
import sysconfig
import threading
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path

__all__ = [
    "QueueFull",
    "Reply",
    "Request",
    "ServiceAlreadyRunning",
    "ServiceClient",
    "ServiceServer",
    "ServiceSpec",
    "ServiceUnavailable",
    "serve",
]

_NATIVE = "running_process._native"


def _load_native():
    """Load ``running_process._native`` without executing ``running_process``.

    ``import running_process._native`` would run the package ``__init__``
    first. Locating the package with ``find_spec`` does not execute it, so
    the extension is loaded straight from the package directory and
    registered under its usual name; a later ``import running_process``
    reuses the same module object.
    """
    module = sys.modules.get(_NATIVE)
    if module is not None:
        return module
    if sysconfig.get_config_var("Py_GIL_DISABLED"):
        raise ImportError(
            "running_process_service needs a GIL-enabled CPython: the published "
            "extension is an ABI3 build of the regular runtime (#1142)"
        )
    package = importlib.util.find_spec("running_process")
    if package is None or not package.submodule_search_locations:
        raise ImportError("running_process is not installed")
    for location in package.submodule_search_locations:
        for suffix in importlib.machinery.EXTENSION_SUFFIXES:
            path = os.path.join(location, "_native" + suffix)
            if os.path.isfile(path):
                loader = importlib.machinery.ExtensionFileLoader(_NATIVE, path)
                spec = importlib.util.spec_from_file_location(_NATIVE, path, loader=loader)
                if spec is None:
                    continue
                module = importlib.util.module_from_spec(spec)
                loader.exec_module(module)
                sys.modules[_NATIVE] = module
                return module
    raise ImportError("running_process._native extension not found")


_native = _load_native()


class ServiceUnavailable(RuntimeError):  # noqa: N818 -- approved public name
    """The service is not connected; the request was not queued."""


class QueueFull(RuntimeError):  # noqa: N818 -- approved public name
    """The client's outgoing queue is full; the request was not queued."""


class ServiceAlreadyRunning(RuntimeError):  # noqa: N818 -- approved public name
    """Another live process already serves this service name."""


@dataclasses.dataclass(frozen=True)
class ServiceSpec:
    """What a service and its clients agree on.

    ``name`` (``[a-z0-9-]{1,64}``) and ``runtime_dir`` determine the
    endpoint and the identity file, so processes built from equal specs meet.
    ``payload_protocol`` is the frame lane: a registered consumer id
    (``0x7000..=0x7EFF``) or private use (``0xF000..=0xFFFF``).
    ``command`` is what a client runs when the service is absent; ``None``
    means clients never spawn (an external supervisor owns the service).
    """

    name: str
    version: str
    payload_protocol: int
    command: Sequence[str] | None = None
    env: Mapping[str, str] | None = None
    cwd: str | os.PathLike[str] | None = None
    runtime_dir: str | os.PathLike[str] | None = None
    health_interval: float = 5.0
    spawn_budget: tuple[int, float] = (3, 30.0)
    spawn_ready_timeout: float = 10.0

    def __post_init__(self) -> None:
        # Validates name and payload protocol (raises ValueError).
        _native.native_service_paths(
            self.name, self.version, self.payload_protocol, self._runtime_dir()
        )

    def _runtime_dir(self) -> str | None:
        return None if self.runtime_dir is None else os.fspath(self.runtime_dir)

    def with_command(self, command: Sequence[str] | None) -> ServiceSpec:
        """A copy of this spec with a different spawn command."""
        return dataclasses.replace(self, command=command)

    @property
    def endpoint(self) -> str:
        """The socket path (Unix) or pipe name (Windows) the service binds."""
        return _native.native_service_paths(
            self.name, self.version, self.payload_protocol, self._runtime_dir()
        )[0]

    @property
    def identity_path(self) -> Path:
        """The identity file the service writes and clients probe."""
        return Path(
            _native.native_service_paths(
                self.name, self.version, self.payload_protocol, self._runtime_dir()
            )[1]
        )


@dataclasses.dataclass(frozen=True)
class Reply:
    """A reply from :meth:`ServiceClient.try_recv`.

    ``payload`` is the service's answer; when it is ``None``, ``error`` says
    why no answer will come (the connection dropped with it in flight).
    """

    request_id: int
    payload: bytes | None
    error: str | None = None


class Request:
    """A request delivered by :meth:`ServiceServer.recv`."""

    __slots__ = ("_native",)

    def __init__(self, native) -> None:
        self._native = native

    @property
    def request_id(self) -> int:
        return self._native.request_id

    @property
    def payload(self) -> bytes:
        return self._native.payload

    def reply(self, payload: bytes | None) -> bool:
        """Send the reply (``None`` sends an empty one). False if the client left."""
        return self._native.reply(b"" if payload is None else bytes(payload))


class ServiceServer:
    """The service's endpoint: bound once per name, per user.

    Raises :class:`ServiceAlreadyRunning` if another live process holds it.
    ``recv`` may be called from several worker threads.
    """

    def __init__(self, spec: ServiceSpec) -> None:
        self.spec = spec
        try:
            self._native = _native.NativeServiceServer(
                spec.name, spec.version, spec.payload_protocol, spec._runtime_dir()
            )
        except FileExistsError as error:
            raise ServiceAlreadyRunning(str(error)) from None

    def recv(self, timeout: float | None = None) -> Request | None:
        """Wait up to ``timeout`` seconds (forever if ``None``) for a request."""
        native = self._native.recv(timeout)
        return None if native is None else Request(native)

    def close(self) -> None:
        """Stop accepting, remove the identity file, release the endpoint."""
        self._native.close()

    def __enter__(self) -> ServiceServer:
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()


def serve(
    spec: ServiceSpec,
    handler: Callable[[bytes], bytes | None],
    *,
    workers: int = 4,
    stop: threading.Event | None = None,
) -> None:
    """Bind ``spec`` and answer requests with ``handler`` until ``stop`` is set.

    ``handler`` runs on one of ``workers`` threads, so a slow request does not
    hold up the rest. An exception in it is reported on stderr and the
    request gets an empty reply.
    """
    stop = stop or threading.Event()
    with ServiceServer(spec) as server:

        def work() -> None:
            while not stop.is_set():
                request = server.recv(0.25)
                if request is None:
                    continue
                try:
                    out = handler(request.payload)
                except KeyboardInterrupt:
                    raise
                except Exception as error:  # report, keep serving
                    print(
                        f"{spec.name}: handler failed: {type(error).__name__}: {error}",
                        file=sys.stderr,
                    )
                    out = None
                request.reply(out)

        threads = [
            threading.Thread(target=work, name=f"{spec.name}-worker-{i}", daemon=True)
            for i in range(max(1, workers))
        ]
        for thread in threads:
            thread.start()
        try:
            while not stop.wait(0.5):
                pass
        finally:
            stop.set()
            for thread in threads:
                thread.join(5)


class ServiceClient:
    """Request/reply client whose every method returns immediately.

    Built with :meth:`connect`. ``send`` queues a request or raises at once;
    ``try_recv`` returns the next reply or ``None``. Poll it from a timer.
    """

    def __init__(self, native) -> None:
        self._native = native

    @classmethod
    def connect(cls, spec: ServiceSpec, *, queue_depth: int = 64) -> ServiceClient:
        """Start supervising ``spec`` in the background and return at once."""
        attempts, window = spec.spawn_budget
        env = None if spec.env is None else [(str(k), str(v)) for k, v in spec.env.items()]
        native = _native.NativeServiceClient(
            spec.name,
            spec.version,
            spec.payload_protocol,
            command=None if spec.command is None else [os.fspath(a) for a in spec.command],
            env=env,
            cwd=None if spec.cwd is None else os.fspath(spec.cwd),
            runtime_dir=spec._runtime_dir(),
            health_interval=float(spec.health_interval),
            spawn_attempts=int(attempts),
            spawn_window=float(window),
            spawn_ready_timeout=float(spec.spawn_ready_timeout),
            queue_depth=int(queue_depth),
        )
        return cls(native)

    @property
    def state(self) -> str:
        """``"starting"``, ``"up"`` or ``"down"``."""
        return self._native.state

    def send(self, payload: bytes) -> int:
        """Queue ``payload`` and return its request id. Never waits.

        Raises :class:`ServiceUnavailable` unless the service is up, and
        :class:`QueueFull` when ``queue_depth`` requests are already waiting.
        """
        rid = self._native.send(payload)
        if rid >= 0:
            return rid
        if rid == -2:
            raise QueueFull("service queue full")
        raise ServiceUnavailable("service unavailable")

    def try_recv(self) -> Reply | None:
        """The oldest reply not yet taken, or ``None``. Never waits."""
        got = self._native.try_recv()
        if got is None:
            return None
        rid, payload, error = got
        return Reply(rid, payload, error)

    def close(self) -> None:
        """Stop supervising and drop the connection. Never waits."""
        self._native.close()
