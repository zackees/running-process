from __future__ import annotations

import tempfile
import unittest
import zipfile
from pathlib import Path

from ci.windows_extension_abi_guard import (
    validate_source_configuration,
    validate_windows_wheel,
)


def write_wheel(path: Path, *, extension: str, tag: str = "cp313-cp313-win_amd64") -> None:
    with zipfile.ZipFile(path, "w") as archive:
        archive.writestr(extension, b"native-extension")
        archive.writestr(
            "running_process-0.0.0.dist-info/WHEEL",
            f"Wheel-Version: 1.0\nTag: {tag}\n",
        )


class TestWindowsExtensionAbiGuard(unittest.TestCase):
    def test_source_configuration_disables_abi3(self) -> None:
        validate_source_configuration()

    def test_accepts_tagged_extension_and_cpython_wheel_tag(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            wheel = Path(directory) / "running_process-test.whl"
            suffix = ".cp313-win_amd64.pyd"
            write_wheel(wheel, extension=f"running_process/_native{suffix}")

            validate_windows_wheel(wheel, extension_suffix=suffix)

    def test_rejects_generic_abi3_extension(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            wheel = Path(directory) / "running_process-test.whl"
            write_wheel(wheel, extension="running_process/_native.pyd", tag="cp313-abi3-win_amd64")

            with self.assertRaisesRegex(RuntimeError, "expected Windows extension"):
                validate_windows_wheel(wheel, extension_suffix=".cp313-win_amd64.pyd")

    def test_rejects_abi3_wheel_tag_even_with_a_tagged_extension(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            wheel = Path(directory) / "running_process-test.whl"
            suffix = ".cp313-win_amd64.pyd"
            write_wheel(
                wheel,
                extension=f"running_process/_native{suffix}",
                tag="cp313-abi3-win_amd64",
            )

            with self.assertRaisesRegex(RuntimeError, "must not advertise the ABI3 tag"):
                validate_windows_wheel(wheel, extension_suffix=suffix)
