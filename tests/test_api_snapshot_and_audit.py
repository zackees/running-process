"""Contract tests for the #875 compatibility gates.

A gate that cannot fail is decoration. These prove the two new ones fail when
they should, and that the checked-in artefacts currently pass.
"""

from __future__ import annotations

import unittest

from ci import api_snapshot, sync_test_audit


class TestApiSnapshot(unittest.TestCase):
    def test_checked_in_snapshots_match_the_current_surface(self) -> None:
        self.assertEqual(api_snapshot.check(), [])

    def test_rendering_is_deterministic(self) -> None:
        # A snapshot that reorders between runs would produce phantom diffs and
        # train reviewers to ignore them.
        self.assertEqual(api_snapshot.render_python(), api_snapshot.render_python())
        self.assertEqual(api_snapshot.render_rust(), api_snapshot.render_rust())

    def test_python_snapshot_records_parameter_names_and_defaults(self) -> None:
        rendered = api_snapshot.render_python()
        # Recording only the method *names* would miss the breakage that
        # actually bites a consumer: a renamed or reordered parameter.
        self.assertIn("def wait(", rendered)
        self.assertIn("timeout=None", rendered)

    def test_python_snapshot_distinguishes_keyword_only_parameters(self) -> None:
        # Moving a parameter across the `*` is a breaking change that a
        # name-only snapshot would not see.
        rendered = api_snapshot._render_signature(
            __import__("ast").parse("def f(a, *, b=1): ...").body[0]
        )
        self.assertEqual(rendered, "def f(a, *, b=1)")

    def test_rust_snapshot_records_full_signatures(self) -> None:
        rendered = api_snapshot.render_rust()
        self.assertIn("pub fn wait(", rendered)
        self.assertIn("Duration", rendered)

    def test_a_changed_signature_is_reported(self) -> None:
        original = api_snapshot.render_python
        api_snapshot.render_python = lambda: "changed\n"  # type: ignore[assignment]
        try:
            failures = api_snapshot.check()
        finally:
            api_snapshot.render_python = original  # type: ignore[assignment]
        self.assertTrue(
            any("does not match the current public surface" in f for f in failures),
            failures,
        )


class TestSyncTestAudit(unittest.TestCase):
    def test_baseline_matches_the_tree(self) -> None:
        self.assertEqual(sync_test_audit.check(), [])

    def test_inventory_excludes_async_tests(self) -> None:
        names, _skipped = sync_test_audit.inventory()
        # From tests/test_async_parity.py: the sync half is inventoried, the
        # async half is not. An audit that counted async tests as sync coverage
        # would report health while sync tests were being replaced by them --
        # exactly the substitution #875 asks us to rule out.
        self.assertIn("test_sync_terminate_ends_the_child_like_kill", names)
        self.assertNotIn("test_async_terminate_ends_the_child_like_kill", names)

    def test_inventory_excludes_tokio_tests(self) -> None:
        names, _skipped = sync_test_audit.inventory()
        self.assertIn("sync_process_close_is_idempotent", names)
        self.assertNotIn("async_process_close_releases_the_actor_and_is_idempotent", names)

    def test_class_level_skips_are_counted(self) -> None:
        # A `@skipUnless` on the class silences every test inside it. Counting
        # only function decorators reported zero skips for a file that was
        # entirely skipped.
        _names, skipped = sync_test_audit.inventory()
        self.assertGreater(len(skipped), 0)

    def test_a_removed_sync_test_is_reported(self) -> None:
        original = sync_test_audit.inventory
        sync_test_audit.inventory = lambda: (set(), set())  # type: ignore[assignment]
        try:
            failures = sync_test_audit.check()
        finally:
            sync_test_audit.inventory = original  # type: ignore[assignment]
        self.assertTrue(
            any("no longer exist" in failure for failure in failures), failures
        )

    def test_adding_a_test_does_not_fail_the_audit(self) -> None:
        # The ratchet is one-way on removal only; new tests must never require
        # a baseline update, or the baseline becomes churn nobody reads.
        original = sync_test_audit.inventory
        names, skipped = original()
        sync_test_audit.inventory = lambda: (names | {"test_brand_new"}, skipped)  # type: ignore[assignment]
        try:
            self.assertEqual(sync_test_audit.check(), [])
        finally:
            sync_test_audit.inventory = original  # type: ignore[assignment]


if __name__ == "__main__":
    unittest.main()
