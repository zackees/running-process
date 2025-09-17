"""Test command line interface (CLI)."""

import subprocess
import sys
import unittest


class TestCLI(unittest.TestCase):
    """Test command line interface functionality."""

    def test_imports(self) -> None:
        """Test command line interface (CLI)."""
        result = subprocess.run(  # noqa: S603
            [sys.executable, "-m", "running_process.cli"],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
