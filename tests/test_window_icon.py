"""Tests for the host window icon API (#577)."""

import unittest

from running_process import window_icon


class TestIconSupport(unittest.TestCase):
    """The capability report, which is the point of the API."""

    def test_support_answers_everywhere(self):
        # Must not raise on a CI box with no console window, on macOS, or
        # anywhere else.
        reason = window_icon.icon_support()
        if reason is not None:
            self.assertTrue(reason, "a refusal must explain itself")
            self.assertIsInstance(reason, str)

    def test_is_supported_agrees_with_the_reason(self):
        reason = window_icon.icon_support()
        self.assertEqual(window_icon.is_supported(), reason is None)


class TestSetHostIcon(unittest.TestCase):
    """Failure behavior, which is what callers must be able to rely on."""

    def test_never_reports_success_for_a_missing_file(self):
        # Whatever the host: an unsupported terminal raises
        # IconUnsupportedError, a supported one fails to load the file. What
        # must never happen is returning None as though it worked.
        with self.assertRaises((window_icon.IconUnsupportedError, OSError)):
            window_icon.set_host_icon("no-such-icon-file.ico")

    def test_unsupported_host_raises_the_typed_error(self):
        if window_icon.is_supported():
            self.skipTest("this host accepts icons; the refusal path needs one that does not")
        with self.assertRaises(window_icon.IconUnsupportedError):
            window_icon.set_host_icon("anything.ico")

    def test_unsupported_error_is_a_runtime_error(self):
        # Callers that only catch RuntimeError should still catch this.
        self.assertTrue(issubclass(window_icon.IconUnsupportedError, RuntimeError))


class TestDegradedBuild(unittest.TestCase):
    """A build without the native symbols must degrade legibly."""

    def test_support_reports_a_reason_when_native_is_absent(self):
        original = window_icon._native_module
        window_icon._native_module = lambda: None
        try:
            reason = window_icon.icon_support()
            self.assertIsNotNone(reason)
            self.assertIn("no window-icon support", reason)
            self.assertFalse(window_icon.is_supported())
        finally:
            window_icon._native_module = original

    def test_setting_raises_when_native_is_absent(self):
        original = window_icon._native_module
        window_icon._native_module = lambda: None
        try:
            with self.assertRaises(window_icon.IconUnsupportedError):
                window_icon.set_host_icon("anything.ico")
        finally:
            window_icon._native_module = original


if __name__ == "__main__":
    unittest.main()
