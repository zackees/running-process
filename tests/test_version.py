"""Unit tests for package version consistency.

These tests ensure version numbers are synchronized across different
configuration files.
"""

import re
import unittest
from pathlib import Path


class TestVersionConsistency(unittest.TestCase):
    """Test that version numbers are consistent across the package."""

    def test_init_and_pyproject_versions_match(self):
        """Test that __version__ in __init__.py matches pyproject.toml version."""
        project_root = Path(__file__).parent.parent

        # Read version from __init__.py
        init_file = project_root / "src" / "running_process" / "__init__.py"
        init_content = init_file.read_text(encoding="utf-8")
        init_match = re.search(r'__version__\s*=\s*"([^"]+)"', init_content)
        self.assertIsNotNone(init_match, "Could not find __version__ in __init__.py")
        assert init_match is not None  # Type guard for pyright
        init_version = init_match.group(1)

        # Read version from pyproject.toml
        pyproject_file = project_root / "pyproject.toml"
        pyproject_content = pyproject_file.read_text(encoding="utf-8")
        pyproject_match = re.search(r'version\s*=\s*"([^"]+)"', pyproject_content)
        self.assertIsNotNone(pyproject_match, "Could not find version in pyproject.toml")
        assert pyproject_match is not None  # Type guard for pyright
        pyproject_version = pyproject_match.group(1)

        # Assert they match
        self.assertEqual(
            init_version,
            pyproject_version,
            f"Version mismatch: __init__.py has {init_version}, pyproject.toml has {pyproject_version}",
        )


if __name__ == "__main__":
    unittest.main()
