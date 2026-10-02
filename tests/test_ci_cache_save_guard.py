"""Tests for ci.cache_save_guard (#1216)."""

import unittest
from pathlib import Path

from ci import cache_save_guard as guard

P = Path("wf.yml")


def check(text: str) -> list[str]:
    return [v.message for v in guard.check_text(P, text)]


class CacheSaveGuardTests(unittest.TestCase):
    def test_rust_cache_is_banned_even_when_main_gated(self) -> None:
        """CACHE-025 (zackees/ci.yml#209): no Swatinem step passes, gated or not."""
        for text in (
            "    steps:\n      - uses: Swatinem/rust-cache@v2\n        with:\n          key: x\n",
            "    steps:\n      - name: c\n        uses: Swatinem/rust-cache@v2\n"
            "        with:\n          save-if: ${{ github.ref == 'refs/heads/main' }}\n",
            "      - uses: Swatinem/rust-cache@v2\n        with:\n          save-if: false\n",
        ):
            messages = check(text)
            self.assertEqual(len(messages), 1)
            self.assertIn("CACHE-025", messages[0])

    def test_setup_soldr_default_save_passes(self) -> None:
        text = "      - uses: zackees/setup-soldr@v0\n        with:\n          cache: true\n"
        self.assertEqual(check(text), [])

    def test_setup_soldr_main_gated_save_passes(self) -> None:
        for gate in (
            "${{ github.ref == 'refs/heads/main' && 'auto' || 'false' }}",
            '"false"',
            "auto",
        ):
            text = (
                "      - name: s\n        uses: zackees/setup-soldr@v0\n"
                f"        with:\n          save-cache: {gate}\n"
            )
            self.assertEqual(check(text), [], gate)

    def test_setup_soldr_unconditional_save_fails(self) -> None:
        text = "      - uses: zackees/setup-soldr@v0\n        with:\n          save-cache: true\n"
        self.assertEqual(len(check(text)), 1)

    def test_save_cache_does_not_leak_from_next_step(self) -> None:
        text = (
            "      - uses: zackees/setup-soldr@v0\n        with:\n          save-cache: true\n"
            "      - uses: zackees/setup-soldr@v0\n        with:\n          save-cache: false\n"
        )
        self.assertEqual(len(check(text)), 1)

    def test_plain_actions_cache_fails(self) -> None:
        text = "      - uses: actions/cache@v4\n        with:\n          path: .venv\n"
        self.assertEqual(len(check(text)), 1)

    def test_restore_only_passes(self) -> None:
        text = "      - uses: actions/cache/restore@v4\n        with:\n          path: .venv\n"
        self.assertEqual(check(text), [])

    def test_save_requires_main_gate(self) -> None:
        bad = "      - uses: actions/cache/save@v4\n        with:\n          path: .venv\n"
        good = (
            "      - if: github.ref == 'refs/heads/main'\n"
            "        uses: actions/cache/save@v4\n"
        )
        self.assertEqual(len(check(bad)), 1)
        self.assertEqual(check(good), [])

    def test_commented_exemption_passes(self) -> None:
        text = (
            "      # cache-save-exempt: tiny, PR-local by design\n"
            "      - uses: actions/cache@v4\n"
        )
        self.assertEqual(check(text), [])

    def test_exemption_needs_reason(self) -> None:
        text = "      # cache-save-exempt:\n      - uses: actions/cache@v4\n"
        self.assertEqual(len(check(text)), 1)

    def test_repository_workflows_pass(self) -> None:
        self.assertEqual(guard.main([]), 0)


if __name__ == "__main__":
    unittest.main()
