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
    def test_rust_cache_only_tracks_the_checkout_target_root(self) -> None:
        """#1173: an absent nested target must not fail post-job cache cleanup."""
        rust_cache = named_workflow_step(
            WORKFLOW.read_text(encoding="utf-8"), "Rust build cache"
        )
        workspaces = [
            line.split(":", maxsplit=1)[1].strip()
            for line in rust_cache
            if re.match(r"^\s*workspaces:\s*", line)
        ]

        self.assertIn("uses: Swatinem/rust-cache@v2", "\n".join(rust_cache))
        self.assertEqual(workspaces, ['". -> target"'])


if __name__ == "__main__":
    unittest.main()
