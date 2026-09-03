from __future__ import annotations

import tempfile
import unittest
import zipfile
from pathlib import Path

from ci.wheel_abi_guard import (
    RELEASE_PLATFORM_TAGS,
    validate_release_coverage,
    validate_source_configuration,
    validate_wheel,
)


def write_wheel(
    path: Path, *, extension: str, tag: str = "cp310-abi3-win_amd64"
) -> None:
    with zipfile.ZipFile(path, "w") as archive:
        archive.writestr(extension, b"native-extension")
        archive.writestr(
            "running_process-0.0.0.dist-info/WHEEL",
            f"Wheel-Version: 1.0\nTag: {tag}\n",
        )


class TestWheelAbiGuard(unittest.TestCase):
    def test_source_configuration_requires_abi3(self) -> None:
        validate_source_configuration()

    def test_accepts_abi3_windows_wheel(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            wheel = Path(directory) / "running_process-test.whl"
            write_wheel(wheel, extension="running_process/_native.pyd")

            validate_wheel(wheel)

    def test_accepts_abi3_unix_wheel(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            wheel = Path(directory) / "running_process-test.whl"
            write_wheel(
                wheel,
                extension="running_process/_native.abi3.so",
                tag="cp310-abi3-macosx_11_0_arm64",
            )

            validate_wheel(wheel)

    def test_rejects_interpreter_specific_wheel(self) -> None:
        """The 4.10.10 regression: a cp311-only wheel installs nowhere else."""
        with tempfile.TemporaryDirectory() as directory:
            wheel = Path(directory) / "running_process-test.whl"
            write_wheel(
                wheel,
                extension="running_process/_native.cp311-win_amd64.pyd",
                tag="cp311-cp311-win_amd64",
            )

            with self.assertRaisesRegex(RuntimeError, "ABI3 native extension"):
                validate_wheel(wheel)

    def test_rejects_abi3_extension_under_an_interpreter_specific_tag(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            wheel = Path(directory) / "running_process-test.whl"
            write_wheel(
                wheel,
                extension="running_process/_native.pyd",
                tag="cp311-cp311-win_amd64",
            )

            with self.assertRaisesRegex(RuntimeError, "must advertise the abi3"):
                validate_wheel(wheel)

    def test_release_coverage_requires_every_platform(self) -> None:
        wheels = [
            Path(f"running_process-1.0.0-cp310-abi3-{tag}.whl")
            for tag in RELEASE_PLATFORM_TAGS
        ]

        validate_release_coverage(wheels)

    def test_release_coverage_rejects_a_missing_macos_wheel(self) -> None:
        wheels = [
            Path(f"running_process-1.0.0-cp310-abi3-{tag}.whl")
            for tag in RELEASE_PLATFORM_TAGS
            if not tag.startswith("macosx")
        ]

        with self.assertRaisesRegex(RuntimeError, "macosx_10_12_x86_64"):
            validate_release_coverage(wheels)


class TestVersionCheckSources(unittest.TestCase):
    """#1189 follow-up: several SOURCES rows never ran.

    `main()` keyed its results by file path, so the four rows that share
    `crates/running-process/Cargo.toml` and
    `crates/running-process-probe-daemon/Cargo.toml` overwrote each other and
    only the last pattern per file was ever compared.
    """

    def test_every_source_row_is_checked(self) -> None:
        from ci.version_check import SOURCES

        self.assertEqual(len(set(SOURCES)), len(SOURCES))
        by_path: dict[str, int] = {}
        for relpath, _pattern in SOURCES:
            by_path[relpath] = by_path.get(relpath, 0) + 1
        self.assertGreater(
            max(by_path.values()),
            1,
            "expected at least one manifest to contribute several version strings",
        )

    def test_the_published_crate_version_is_a_source(self) -> None:
        from ci.version_check import SOURCES

        self.assertIn(
            ("crates/running-process/Cargo.toml", r'^version\s*=\s*"([^"]+)"'),
            SOURCES,
        )
