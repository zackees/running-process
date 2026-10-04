"""Regression contracts for the reusable preflight workflow."""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github" / "workflows" / "ci-preflight.yml"


def named_workflow_step(workflow: str, name: str) -> list[str]:
    """Return the uniquely named YAML list item without assuming indentation."""
    lines = workflow.splitlines()
    step_starts = [
        index
        for index, line in enumerate(lines)
        if re.fullmatch(rf"(?P<indent>\s*)-\s*name:\s*{re.escape(name)}\s*", line)
    ]
    if len(step_starts) != 1:
        raise AssertionError(f"expected one {name!r} workflow step, found {len(step_starts)}")

    start = step_starts[0]
    indent = len(lines[start]) - len(lines[start].lstrip())
    end = next(
        (
            index
            for index in range(start + 1, len(lines))
            if re.match(rf"^\s{{{indent}}}-\s", lines[index])
        ),
        len(lines),
    )
    return lines[start:end]


class TestPreflightWorkflowContract(unittest.TestCase):
    def test_rust_build_cache_is_setup_soldr(self) -> None:
        """CACHE-025: the build cache is setup-soldr's, never Swatinem's."""
        workflow = WORKFLOW.read_text(encoding="utf-8")
        self.assertNotIn("Swatinem/rust-cache", workflow)
        setup = "\n".join(named_workflow_step(workflow, "Set up soldr (toolchain + build cache)"))
        self.assertIn("uses: zackees/setup-soldr@v0", setup)
        self.assertIn("cache: true", setup)
        self.assertNotIn("cache: false", setup)
        self.assertIn(
            "save-cache: auto",
            setup,
        )
        self.assertIn(
            "save-cache-remote: ${{ github.ref == 'refs/heads/main' && 'auto' || 'false' }}",
            setup,
        )
        # The workspace build has to go through soldr to reach the cache.
        build = "\n".join(
            named_workflow_step(
                workflow,
                "cargo build (workspace + all-targets, debug, --features client)",
            )
        )
        self.assertIn("run: soldr cargo build --workspace", build)


    def test_build_output_writer_receives_terminal_job_status(self) -> None:
        """CACHE-008: local retention must still refuse a failed build."""
        setup = "\n".join(named_workflow_step(
            WORKFLOW.read_text(encoding="utf-8"),
            "Set up soldr (toolchain + build cache)",
        ))
        self.assertIn("job-status: ${{ job.status }}", setup)


    def test_test_domain_excludes_bulk_snapshots_without_serializing_nextest(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        setup = "\n".join(named_workflow_step(
            workflow, "Set up soldr (toolchain + build cache)",
        ))
        self.assertIn("ci-tests: true", setup)
        self.assertIn('NEXTEST_TEST_THREADS: "num-cpus"', workflow)


if __name__ == "__main__":
    unittest.main()
