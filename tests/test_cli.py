"""Test command line interface (CLI)."""

import subprocess
import sys


def test_imports() -> None:
    """Test command line interface (CLI)."""
    result = subprocess.run(  # noqa: S603
        [sys.executable, "-m", "running_process.cli"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0


if __name__ == "__main__":
    test_imports()
    print("CLI test passed!")  # noqa: T201
