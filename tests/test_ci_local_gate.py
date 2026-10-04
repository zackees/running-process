"""Source and executed-step evidence must fail closed before attesting a gate."""

from __future__ import annotations

import json
import unittest
from dataclasses import dataclass
from pathlib import Path
from unittest.mock import patch

from ci import local_gate

FIXTURE = Path(__file__).parent / "fixtures" / "bosn_linux_quick_receipt.json"


@dataclass(frozen=True)
class FieldCase:
    field: str
    values: tuple[local_gate.JsonValue, ...]


@dataclass(frozen=True)
class FidelityCase:
    host: str
    daemon: str
    code: int


class LocalGateProofTests(unittest.TestCase):
    def setUp(self) -> None:
        self.document: dict[str, local_gate.JsonValue] = json.loads(FIXTURE.read_text())

    def error(self) -> str | None:
        return local_gate.proof_error(
            json.dumps(self.document),
            workspace=Path("/work/repo"),
            head_sha="a" * 40,
        )

    def test_complete_native_receipt_proves_the_selected_quick_gate(self) -> None:
        self.assertIsNone(self.error())

    def test_local_execution_uses_quick_dispatch_before_attesting(self) -> None:
        command = local_gate.command()
        self.assertIn("--event", command)
        self.assertEqual(command[command.index("--event") + 1], "workflow_dispatch")
        self.assertIn("full=false", command)
        self.document["event"] = "workflow_dispatch"
        self.assertIsNone(self.error())

    def test_integration_label_preserves_additional_remote_coverage(self) -> None:
        trust = (local_gate.ROOT / "local-gate.toml").read_text().split("[gate.trust]", 1)[1]
        labels = next(line for line in trust.splitlines() if line.startswith("full-labels ="))
        self.assertIn("ci-integration", json.loads(labels.split("=", 1)[1]))
        workflow = (local_gate.ROOT / local_gate.WORKFLOW).read_text()
        self.assertIn("integration-test:", workflow)
        self.assertIn("'ci-integration'", workflow)

    def test_wrong_source_and_selection_never_prove_this_gate(self) -> None:
        cases = (
            FieldCase("workspace", ("/work/another-repo", "relative/repo")),
            FieldCase("sha", ("b" * 40, None)),
            FieldCase("dirty", (False, "digest")),
            FieldCase("state", ("queued", "running")),
            FieldCase("conclusion", ("failure", "cancelled", None)),
            FieldCase("exit_code", (1, None, False)),
            FieldCase("engine", ("docker", None)),
            FieldCase("event", ("push", "pull_request")),
            FieldCase("workflow", (".github/workflows/ci-linux.yml",)),
            FieldCase("job", (None, "linux-full")),
            FieldCase("mode", ("full", None)),
            FieldCase("act_version", ("0.2.89", "unknown", None)),
        )
        for case in cases:
            original = self.document[case.field]
            for value in case.values:
                with self.subTest(field=case.field, value=value):
                    self.document[case.field] = value
                    self.assertIsNotNone(self.error())
            self.document[case.field] = original

    def test_missing_skipped_failed_or_unexecuted_required_steps_fail(self) -> None:
        tree = local_gate.document(self.document["tree"])
        group = local_gate.document(local_gate.values(tree["groups"])[0])
        jobs = local_gate.values(group["jobs"])
        for job_value in jobs:
            job = local_gate.document(job_value)
            sections = local_gate.values(job["sections"])
            for section_value in sections:
                section = local_gate.document(section_value)
                for field, invalid in {
                    "stage": "Pre",
                    "status": "running",
                    "conclusion": "skipped",
                }.items():
                    original = section[field]
                    section[field] = invalid
                    self.assertIsNotNone(self.error())
                    section[field] = original
                section["conclusion"] = "failure"
                self.assertIsNotNone(self.error())
                section["conclusion"] = "success"
            job["sections"] = []
            self.assertIsNotNone(self.error())
            job["sections"] = sections

    def test_duplicate_missing_or_failed_jobs_fail(self) -> None:
        tree = local_gate.document(self.document["tree"])
        group = local_gate.document(local_gate.values(tree["groups"])[0])
        jobs = local_gate.values(group["jobs"])
        group["jobs"] = [*jobs, jobs[0]]
        self.assertIsNotNone(self.error())
        group["jobs"] = jobs[:-1]
        self.assertIsNotNone(self.error())
        group["jobs"] = jobs
        job = local_gate.document(jobs[0])
        job["conclusion"] = "failure"
        self.assertIsNotNone(self.error())

    def test_malformed_or_ambiguous_receipts_fail(self) -> None:
        for output in (
            "",
            "{}",
            "{broken",
            json.dumps(self.document) + "\n" + json.dumps(self.document),
        ):
            self.assertIsNotNone(
                local_gate.proof_error(
                    output,
                    workspace=Path("/work/repo"),
                    head_sha="a" * 40,
                )
            )

    def test_native_linux_x64_is_required_for_the_test_lane(self) -> None:
        self.assertIsNone(local_gate.fidelity_error("x86_64", "linux x86_64", 0))
        cases = (
            FidelityCase("aarch64", "linux x86_64", 0),
            FidelityCase("x86_64", "windows amd64", 0),
            FidelityCase("x86_64", "linux aarch64", 0),
            FidelityCase("x86_64", "linux x86_64", 1),
        )
        for case in cases:
            self.assertIsNotNone(local_gate.fidelity_error(case.host, case.daemon, case.code))

    def test_failed_boundary_check_prevents_engine_submission(self) -> None:
        with (
            patch.object(local_gate, "_head", return_value="a" * 40),
            patch.object(
                local_gate, "run_captured", return_value=local_gate.Captured(1, "ledger drift")
            ) as run,
        ):
            self.assertEqual(1, local_gate.main([]))
        run.assert_called_once_with([local_gate.sys.executable, "-m", "ci.platform_boundary"])

    def test_failed_async_check_prevents_engine_submission(self) -> None:
        with (
            patch.object(local_gate, "_head", return_value="a" * 40),
            patch.object(
                local_gate,
                "run_captured",
                side_effect=[local_gate.Captured(0, ""), local_gate.Captured(1, "stdio debt")],
            ) as run,
        ):
            self.assertEqual(1, local_gate.main([]))
        self.assertEqual(2, run.call_count)
        run.assert_called_with([local_gate.sys.executable, "-m", "ci.async_compliance_guard"])


if __name__ == "__main__":
    unittest.main()
