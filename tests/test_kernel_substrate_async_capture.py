"""Collected command and workflow contracts for #1155 semantic capture."""

from __future__ import annotations

import unittest
from pathlib import Path

from ci.__main__ import STAGES
from ci.kernel_substrate_async_capture import contract_command, fixture_command

ROOT = Path(__file__).resolve().parent.parent


class KernelSubstrateAsyncCaptureTests(unittest.TestCase):
    def test_dispatcher_exposes_the_dedicated_contract_stage(self) -> None:
        self.assertEqual(
            STAGES["test-async-semantic-capture"], "ci.kernel_substrate_async_capture"
        )

    def test_contract_builds_testbins_then_exact_minimal_feature_test(self) -> None:
        self.assertEqual(fixture_command()[-3:], ("build", "-p", "testbins"))
        self.assertEqual(
            contract_command()[-8:],
            (
                "test",
                "-p",
                "running-process",
                "--no-default-features",
                "--features",
                "kernel-substrate",
                "--test",
                "async_semantic_capture",
            ),
        )

    def test_normal_preflight_invokes_the_dispatcher_stage(self) -> None:
        workflow = (ROOT / ".github" / "workflows" / "ci-preflight.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("Async semantic capture contract (kernel substrate)", workflow)
        self.assertIn("ci test-async-semantic-capture", workflow)
