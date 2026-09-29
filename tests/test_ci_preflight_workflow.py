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
        raise AssertionError(
            f"expected one {name!r} workflow step, found {len(step_starts)}"
        )

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
    def test_rust_cache_only_tracks_created_target_roots(self) -> None:
        """#1173: every listed target must exist before post-job cache cleanup."""
        workflow = WORKFLOW.read_text(encoding="utf-8")
        rust_cache = named_workflow_step(workflow, "Rust build cache")
        start = next(
            index
            for index, line in enumerate(rust_cache)
            if re.match(r"^\s*workspaces:\s*\|\s*$", line)
        )
        indent = len(rust_cache[start + 1]) - len(rust_cache[start + 1].lstrip())
        workspaces = []
        for line in rust_cache[start + 1 :]:
            if not line.strip() or len(line) - len(line.lstrip()) < indent:
                break
            workspaces.append(line.strip())

        self.assertIn("uses: Swatinem/rust-cache@v2", "\n".join(rust_cache))
        self.assertEqual(workspaces, [". -> target", "testbins-tokio -> target"])
        # The nested fixture target is only built in some lanes; it must be
        # created before rust-cache registers it.
        mkdir = named_workflow_step(workflow, "Create tokio fixture target dir")
        self.assertIn("run: mkdir -p testbins-tokio/target", "\n".join(mkdir))
        self.assertLess(
            workflow.index("- name: Create tokio fixture target dir"),
            workflow.index("- name: Rust build cache"),
        )


if __name__ == "__main__":
    unittest.main()
