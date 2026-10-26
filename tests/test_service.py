"""Supervised direct-endpoint services: Python service + non-blocking client.

Covers ``running_process_service``: a Python service registers on a
per-user endpoint (one instance per name), answers framed requests, and
is supervised by its clients -- health-probed through its identity file
and respawned under a lock within a spawn budget. The client never
blocks: ``send`` fails fast when the service is down or the queue is
full, and replies are collected with ``try_recv`` and matched by id.
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
import unittest
from pathlib import Path

PROTOCOL = 0xF0B7
# Any call on the client that is meant to be non-blocking must return
# well inside one kitty frame; 50 ms is generous on a loaded CI runner.
NON_BLOCKING_BUDGET = 0.05


def _wait_until(predicate, timeout: float = 15.0, interval: float = 0.01):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(interval)
    raise AssertionError(f"condition not met within {timeout}s")


def _recv(client, timeout: float = 15.0):
    return _wait_until(client.try_recv, timeout)


class _Runtime(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="rpsvc-")
        self.runtime = Path(self._tmp.name)
        # Cleanups run last-registered-first, after tearDown. Registering the
        # directory first makes it the very last thing removed: a client
        # closed in its own cleanup can no longer respawn a service into it.
        self.addCleanup(self._tmp.cleanup)

    def spec(self, name: str = "svc-test", **kwargs):
        from running_process_service import ServiceSpec

        return ServiceSpec(
            name=name,
            version="1.0.0",
            payload_protocol=PROTOCOL,
            runtime_dir=self.runtime,
            **kwargs,
        )


class ImportTest(unittest.TestCase):
    def test_import_does_not_load_running_process_package(self) -> None:
        code = (
            "import sys, running_process_service as s; "
            "assert 'running_process' not in sys.modules, sorted(sys.modules); "
            "print(s.ServiceClient.__name__)"
        )
        out = subprocess.run(
            [sys.executable, "-c", code], capture_output=True, text=True, timeout=60, check=False
        )
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(out.stdout.strip(), "ServiceClient")


class SpecTest(_Runtime):
    def test_rejects_bad_name_and_first_party_protocol(self) -> None:
        from running_process_service import ServiceSpec

        with self.assertRaises(ValueError):
            ServiceSpec(name="Bad Name", version="1", payload_protocol=PROTOCOL)
        with self.assertRaises(ValueError):
            ServiceSpec(name="ok", version="1", payload_protocol=0xAD01)
        with self.assertRaises(ValueError):
            ServiceSpec(name="ok", version="1", payload_protocol=0x1234)


class InProcessTest(_Runtime):
    def _serve(self, spec, handler, workers: int = 4):
        from running_process_service import ServiceServer

        server = ServiceServer(spec)
        stop = threading.Event()

        def loop() -> None:
            while not stop.is_set():
                request = server.recv(0.05)
                if request is not None:
                    request.reply(handler(request.payload))

        threads = [threading.Thread(target=loop, daemon=True) for _ in range(workers)]
        for t in threads:
            t.start()

        def close() -> None:
            stop.set()
            for t in threads:
                t.join(5)
            server.close()

        self.addCleanup(close)
        return server

    def _client(self, spec, **kwargs):
        from running_process_service import ServiceClient

        client = ServiceClient.connect(spec, **kwargs)
        self.addCleanup(client.close)
        return client

    def test_round_trip_matches_reply_to_request_id(self) -> None:
        spec = self.spec()
        self._serve(spec, lambda payload: b"pong:" + payload)
        client = self._client(spec)
        _wait_until(lambda: client.state == "up")
        rid = client.send(b"ping")
        reply = _recv(client)
        self.assertEqual(reply.request_id, rid)
        self.assertIsNone(reply.error)
        self.assertEqual(reply.payload, b"pong:ping")

    def test_out_of_order_replies_are_matched_by_id(self) -> None:
        spec = self.spec()
        release = threading.Event()

        def handler(payload: bytes) -> bytes:
            if payload == b"slow":
                release.wait(10)
            return payload

        self._serve(spec, handler)
        client = self._client(spec)
        _wait_until(lambda: client.state == "up")
        slow = client.send(b"slow")
        fast = client.send(b"fast")
        first = _recv(client)
        self.assertEqual((first.request_id, first.payload), (fast, b"fast"))
        release.set()
        second = _recv(client)
        self.assertEqual((second.request_id, second.payload), (slow, b"slow"))

    def test_second_server_is_refused(self) -> None:
        from running_process_service import ServiceAlreadyRunning, ServiceServer

        spec = self.spec()
        self._serve(spec, lambda p: p)
        with self.assertRaises(ServiceAlreadyRunning):
            ServiceServer(spec)

    def test_send_fails_fast_when_service_is_down(self) -> None:
        from running_process_service import ServiceUnavailable

        client = self._client(self.spec())
        started = time.monotonic()
        with self.assertRaises(ServiceUnavailable):
            client.send(b"x")
        self.assertLess(time.monotonic() - started, NON_BLOCKING_BUDGET)
        started = time.monotonic()
        self.assertIsNone(client.try_recv())
        self.assertLess(time.monotonic() - started, NON_BLOCKING_BUDGET)
        self.assertIn(client.state, ("starting", "down"))

    def test_send_fails_fast_when_queue_is_full(self) -> None:
        from running_process_service import QueueFull

        spec = self.spec()
        release = threading.Event()
        self.addCleanup(release.set)
        self._serve(spec, lambda p: (release.wait(10), p)[1], workers=1)
        client = self._client(spec, queue_depth=2)
        _wait_until(lambda: client.state == "up")
        worst = 0.0
        with self.assertRaises(QueueFull):
            for _ in range(100_000):
                started = time.monotonic()
                try:
                    client.send(b"x" * 65536)
                finally:
                    worst = max(worst, time.monotonic() - started)
        self.assertLess(worst, NON_BLOCKING_BUDGET)

    def test_client_close_never_waits(self) -> None:
        from running_process_service import ServiceUnavailable

        spec = self.spec()
        self._serve(spec, lambda p: p)
        client = self._client(spec)
        _wait_until(lambda: client.state == "up")
        started = time.monotonic()
        client.close()
        self.assertLess(time.monotonic() - started, NON_BLOCKING_BUDGET)
        self.assertEqual(client.state, "down")
        with self.assertRaises(ServiceUnavailable):
            client.send(b"x")

    def test_server_close_releases_the_endpoint(self) -> None:
        from running_process_service import ServiceServer

        spec = self.spec()
        first = ServiceServer(spec)
        self.assertTrue(spec.identity_path.exists())
        first.close()
        self.assertFalse(spec.identity_path.exists())
        second = ServiceServer(spec)
        second.close()

    def test_server_restart_is_recovered_without_new_client(self) -> None:
        from running_process_service import ServiceServer

        spec = self.spec(health_interval=0.2)
        server = ServiceServer(spec)
        client = self._client(spec)
        _wait_until(lambda: client.state == "up")
        server.close()
        _wait_until(lambda: client.state != "up")
        self._serve(spec, lambda p: b"again:" + p)
        _wait_until(lambda: client.state == "up")
        rid = client.send(b"x")
        reply = _recv(client)
        self.assertEqual((reply.request_id, reply.payload), (rid, b"again:x"))


SERVICE_SCRIPT = textwrap.dedent(
    """
    import os, sys
    from pathlib import Path
    from running_process_service import ServiceSpec, serve

    runtime = Path(sys.argv[1])
    with open(runtime / "spawns.log", "a") as f:
        f.write(f"{os.getpid()}\\n")
    if len(sys.argv) > 2 and sys.argv[2] == "crash":
        sys.exit(3)
    spec = ServiceSpec(name="svc-spawn", version="1.0.0", payload_protocol=0xF0B7,
                       runtime_dir=runtime)
    serve(spec, lambda payload: str(os.getpid()).encode())
    """
)


class SupervisionTest(_Runtime):
    def setUp(self) -> None:
        super().setUp()
        self.script = self.runtime / "service.py"
        self.script.write_text(SERVICE_SCRIPT, encoding="utf-8")
        self.pids: list[int] = []
        # Before the directory goes, after every client is closed (clients
        # register their close later, so it runs first): otherwise a live
        # supervisor sees the kill and respawns a service mid-rmtree.
        self.addCleanup(self._kill_services)

    def _kill_services(self) -> None:
        for pid in self.pids:
            try:
                os.kill(pid, 9)
            except OSError:
                pass

    def spawns(self) -> list[int]:
        log = self.runtime / "spawns.log"
        if not log.exists():
            return []
        return [int(x) for x in log.read_text().split()]

    def client(self, *extra: str, **kwargs):
        from running_process_service import ServiceClient

        spec = self.spec(
            name="svc-spawn",
            command=[sys.executable, str(self.script), str(self.runtime), *extra],
            health_interval=0.2,
            **kwargs,
        )
        client = ServiceClient.connect(spec)
        self.addCleanup(client.close)
        return client

    def ask_pid(self, client) -> int:
        _wait_until(lambda: client.state == "up", 30)
        rid = client.send(b"pid")
        reply = _recv(client)
        self.assertEqual(reply.request_id, rid)
        pid = int(reply.payload)
        self.pids.append(pid)
        return pid

    def test_client_spawns_service_and_respawns_after_crash(self) -> None:
        client = self.client()
        first = self.ask_pid(client)
        os.kill(first, 9)
        _wait_until(lambda: client.state != "up")
        second = self.ask_pid(client)
        self.assertNotEqual(first, second)

    def test_two_clients_share_one_instance(self) -> None:
        a = self.client()
        b = self.client()
        self.assertEqual(self.ask_pid(a), self.ask_pid(b))
        self.assertEqual(len(set(self.spawns())), 1)

    def test_spawn_budget_limits_crash_loop(self) -> None:
        self.client("crash", spawn_budget=(3, 30.0))
        _wait_until(lambda: len(self.spawns()) >= 3, 30)
        time.sleep(2.0)
        self.assertEqual(len(self.spawns()), 3)


if __name__ == "__main__":
    unittest.main()
