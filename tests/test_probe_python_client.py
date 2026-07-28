"""Tests for the Python probe client (#634)."""

import threading
import time
import unittest

import pytest

from running_process import probe

requires_probe = pytest.mark.skipif(
    not probe.is_available(),
    reason="this build of _native has no probe support",
)

# A socket that deliberately does not exist. Enrollment must succeed anyway —
# an absent daemon is a normal condition the worker retries through — so this
# keeps the tests independent of whether a daemon happens to be running.
NO_DAEMON = "\\\\.\\pipe\\rp-probe-test-nonexistent"


class TestProbeConfig(unittest.TestCase):
    """Config defaults, which are security-relevant."""

    def test_env_values_are_deny_by_default(self):
        config = probe.ProbeConfig(app_class="t")
        self.assertEqual(config.env_allowlist, [])
        self.assertFalse(config.disclose_cwd)

    def test_allowlists_are_not_shared_between_configs(self):
        # A mutable default would let one config's opt-in leak into every
        # other config in the process.
        a = probe.ProbeConfig(app_class="a")
        b = probe.ProbeConfig(app_class="b")
        a.env_allowlist.append("SECRET")
        self.assertEqual(b.env_allowlist, [])


@requires_probe
class TestProbeInstall(unittest.TestCase):
    """Enrollment lifecycle against an absent daemon."""

    def test_install_does_not_block(self):
        start = time.monotonic()
        guard = probe.install(
            probe.ProbeConfig(app_class="t", socket_override=NO_DAEMON)
        )
        elapsed = time.monotonic() - start
        self.addCleanup(guard.close)

        # The target is well under a millisecond; the bound is loose so a
        # loaded CI runner does not make this flaky. It still fails outright
        # if enrollment ever waits on the daemon.
        self.assertLess(
            elapsed,
            1.0,
            f"install() took {elapsed:.3f}s; enrollment must not do I/O",
        )

    def test_install_succeeds_without_a_daemon(self):
        guard = probe.install(
            probe.ProbeConfig(app_class="t", socket_override=NO_DAEMON)
        )
        self.addCleanup(guard.close)
        self.assertIsNotNone(guard)
        self.assertIsNotNone(guard.handle)
        # Enrolling is not the same as being registered: no daemon answered.
        self.assertFalse(guard.is_armed())

    def test_close_is_idempotent(self):
        guard = probe.install(
            probe.ProbeConfig(app_class="t", socket_override=NO_DAEMON)
        )
        self.assertTrue(guard.close(), "the first close releases the guard")
        self.assertFalse(guard.close(), "a second close releases nothing")
        self.assertIsNone(guard.handle)

    def test_closed_guard_is_not_armed(self):
        guard = probe.install(
            probe.ProbeConfig(app_class="t", socket_override=NO_DAEMON)
        )
        guard.close()
        self.assertFalse(guard.is_armed())

    def test_context_manager_closes(self):
        with probe.install(
            probe.ProbeConfig(app_class="t", socket_override=NO_DAEMON)
        ) as guard:
            self.assertIsNotNone(guard.handle)
        self.assertIsNone(guard.handle)

    def test_guards_are_independent(self):
        first = probe.install(
            probe.ProbeConfig(app_class="a", socket_override=NO_DAEMON)
        )
        second = probe.install(
            probe.ProbeConfig(app_class="b", socket_override=NO_DAEMON)
        )
        self.addCleanup(second.close)

        self.assertNotEqual(first.handle, second.handle)
        first.close()
        self.assertIsNotNone(second.handle, "closing one guard must not close another")
        self.assertTrue(second.close())

    def test_close_from_another_thread_releases_once(self):
        # atexit and an explicit close can race; exactly one must win.
        guard = probe.install(
            probe.ProbeConfig(app_class="t", socket_override=NO_DAEMON)
        )
        results = []

        def closer():
            results.append(guard.close())

        threads = [threading.Thread(target=closer) for _ in range(4)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        self.assertEqual(
            sum(1 for r in results if r),
            1,
            f"exactly one close should do the release, got {results}",
        )


class TestProbeUnavailable(unittest.TestCase):
    """Degrading when the wheel was built without probe support."""

    def test_install_returns_none_when_unavailable(self):
        original = probe._native_module
        probe._native_module = lambda: None
        try:
            result = probe.install(probe.ProbeConfig(app_class="t"))
            self.assertIsNone(result, "a probe-less build degrades rather than raising")
        finally:
            probe._native_module = original

    def test_required_turns_unavailability_into_an_error(self):
        original = probe._native_module
        probe._native_module = lambda: None
        try:
            with self.assertRaises(probe.ProbeUnavailableError):
                probe.install(probe.ProbeConfig(app_class="t"), required=True)
        finally:
            probe._native_module = original


if __name__ == "__main__":
    unittest.main()
