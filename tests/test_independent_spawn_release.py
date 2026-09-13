"""Release constraints introduced by the independent broker protocol edge."""

import re
import unittest
from pathlib import Path

from ci import version_check

ROOT = Path(__file__).resolve().parents[1]


class IndependentSpawnReleaseTests(unittest.TestCase):
    def test_workflow_publishes_protocol_before_platform(self):
        workflow = (ROOT / ".github/workflows/auto-release.yml").read_text(
            encoding="utf-8"
        )
        preflight = re.search(r"rust_crates = \((.*?)\n\s*\)", workflow, re.S)
        publish = re.search(r"PUBLISH_ORDER=\((.*?)\n\s*\)", workflow, re.S)
        self.assertIsNotNone(preflight)
        self.assertIsNotNone(publish)
        assert preflight is not None
        assert publish is not None
        expected = [
            "running-process-probe",
            "running-process-protocol",
            "running-process-platform-internal",
            "running-process",
            "running-process-probe-daemon",
            "running-process-py",
        ]
        self.assertEqual(re.findall(r'"([^"]+)"', preflight.group(1)), expected)
        self.assertEqual(publish.group(1).split(), expected)

    def test_version_gate_tracks_platform_protocol_pin(self):
        manifest = "crates/running-process-platform-internal/Cargo.toml"
        matches = [
            pattern for path, pattern in version_check.SOURCES if path == manifest
        ]
        self.assertTrue(
            matches, "platform's optional protocol dependency must stay lockstep"
        )
        for pattern in matches:
            value = version_check._extract_version(ROOT / manifest, pattern)
            workspace = version_check._extract_version(
                ROOT / "Cargo.toml", r'^version\s*=\s*"([^"]+)"'
            )
            self.assertEqual(value, workspace)


if __name__ == "__main__":
    unittest.main()
