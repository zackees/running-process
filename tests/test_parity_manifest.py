"""Contract tests for the #875 sync/async parity gate itself.

The manifest is only worth having if it fails when it should. These exercise the
three ways it is meant to fail -- an undocumented public member, a citation to a
test that does not exist, and a planned row once the end-state switch is on --
plus the drift check against the real tree.
"""

from __future__ import annotations

import unittest

from ci import parity_manifest

HEADER = "[settings]\nrequire_no_planned = false\n"


def _row(**fields: str) -> str:
    lines = ["[[row]]"]
    lines.extend(f'{key} = "{value}"' for key, value in fields.items())
    return "\n".join(lines) + "\n"


class TestManifestParsing(unittest.TestCase):
    def test_settings_and_rows_round_trip(self) -> None:
        manifest = parity_manifest.parse_manifest(
            "[settings]\nrequire_no_planned = true\n"
            + _row(
                id="rust-process.kill",
                surface="rust-process",
                member="kill",
                status="implemented",
                rust_sync="some_test",
                rust_async="n/a: rationale",
            )
        )
        self.assertTrue(manifest.require_no_planned)
        self.assertEqual(len(manifest.rows), 1)
        row = manifest.rows[0]
        self.assertEqual(row.member, "kill")
        self.assertEqual(row.columns["rust_sync"], "some_test")
        self.assertEqual(row.columns["python_async"], "")

    def test_key_outside_a_row_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            parity_manifest.parse_manifest('id = "orphan"\n')


class TestSurfaceDiscovery(unittest.TestCase):
    def test_rust_impl_block_yields_public_methods_only(self) -> None:
        source = "impl Thing {\n    pub fn kept(&self) {}\n    fn private(&self) {}\n}\n"
        self.assertEqual(parity_manifest._rust_members(source, "Thing"), {"kept"})

    def test_rust_scanner_sees_async_and_restricted_visibility(self) -> None:
        source = (
            "impl Thing {\n"
            "    pub async fn awaited(&self) {}\n"
            "    pub(crate) fn scoped(&self) {}\n"
            "}\n"
        )
        self.assertEqual(
            parity_manifest._rust_members(source, "Thing"), {"awaited", "scoped"}
        )

    def test_rust_scanner_ignores_functions_after_the_impl_block(self) -> None:
        source = "impl Thing {\n    pub fn inside(&self) {}\n}\npub fn outside() {}\n"
        self.assertEqual(parity_manifest._rust_members(source, "Thing"), {"inside"})

    def test_python_class_yields_public_methods_only(self) -> None:
        source = "class Thing:\n    def kept(self): ...\n    def _private(self): ...\n"
        self.assertEqual(parity_manifest._python_members(source, "Thing"), {"kept"})

    def test_missing_symbol_is_an_error_not_an_empty_surface(self) -> None:
        # Silently returning an empty set would let a renamed class disable the
        # coverage check for its whole surface.
        with self.assertRaises(ValueError):
            parity_manifest._rust_members("impl Other {\n}\n", "Thing")
        with self.assertRaises(ValueError):
            parity_manifest._python_members("class Other:\n    pass\n", "Thing")


class TestGateFailures(unittest.TestCase):
    def test_implemented_row_citing_a_missing_test_fails(self) -> None:
        rows = [
            parity_manifest.Row(
                identifier="rust-process.kill",
                surface="rust-process",
                member="kill",
                status="implemented",
                columns={
                    "rust_sync": "definitely_not_a_real_test_name_xyzzy",
                    "rust_async": "n/a: reason",
                    "python_sync": "n/a: reason",
                    "python_async": "n/a: reason",
                },
            )
        ]
        failures = _run_gate(rows)
        self.assertTrue(
            any("does not exist in the tree" in failure for failure in failures),
            failures,
        )

    def test_na_without_a_rationale_fails(self) -> None:
        rows = [
            parity_manifest.Row(
                identifier="rust-process.kill",
                surface="rust-process",
                member="kill",
                status="implemented",
                columns={column: "n/a:" for column in parity_manifest.COLUMNS},
            )
        ]
        self.assertTrue(
            any("without a rationale" in failure for failure in _run_gate(rows))
        )

    def test_uncovered_public_member_fails(self) -> None:
        # An empty manifest means every discovered member is undocumented.
        failures = _run_gate([])
        self.assertTrue(any("has no parity row" in failure for failure in failures))

    def test_row_for_a_nonexistent_member_fails(self) -> None:
        rows = [
            parity_manifest.Row(
                identifier="rust-process.ghost",
                surface="rust-process",
                member="ghost_method_that_was_removed",
                status="planned",
                issue="875",
            )
        ]
        self.assertTrue(
            any("no longer\nexists" in f or "no longer" in f for f in _run_gate(rows))
        )

    def test_planned_row_without_an_issue_fails(self) -> None:
        rows = [
            parity_manifest.Row(
                identifier="rust-process.kill",
                surface="rust-process",
                member="kill",
                status="planned",
            )
        ]
        self.assertTrue(any("owning `issue`" in failure for failure in _run_gate(rows)))


def _run_gate(rows: list[parity_manifest.Row]) -> list[str]:
    """Run `check()` against a synthetic manifest instead of the real file."""
    original = parity_manifest.parse_manifest
    manifest = parity_manifest.Manifest(require_no_planned=False, rows=rows)
    parity_manifest.parse_manifest = lambda _text: manifest  # type: ignore[assignment]
    try:
        failures, _ = parity_manifest.check()
    finally:
        parity_manifest.parse_manifest = original  # type: ignore[assignment]
    return failures


class TestCheckedInManifest(unittest.TestCase):
    def test_repository_manifest_passes_its_own_gate(self) -> None:
        failures, _planned = parity_manifest.check()
        self.assertEqual(failures, [])

    def test_strict_mode_reports_every_planned_row(self) -> None:
        _failures, planned = parity_manifest.check()
        strict_failures, _ = parity_manifest.check(strict=True)
        # `--strict` is the RED view: exactly the planned rows, no more.
        self.assertEqual(len(strict_failures), len(planned))

    def test_generated_document_matches_the_manifest(self) -> None:
        manifest = parity_manifest.parse_manifest(
            parity_manifest.MANIFEST.read_text(encoding="utf-8")
        )
        self.assertEqual(parity_manifest._check_document(manifest), [])


if __name__ == "__main__":
    unittest.main()
