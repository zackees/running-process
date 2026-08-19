from __future__ import annotations

import json
import subprocess
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest.mock import patch

from ci import build_wheel


class TrampolineWheelTest(unittest.TestCase):
    def test_accepts_the_platform_trampoline_entry(self) -> None:
        suffix = ".exe" if build_wheel.platform.system() == "Windows" else ""
        expected = f"running_process/assets/daemon-trampoline{suffix}"
        with tempfile.TemporaryDirectory() as directory:
            wheel = Path(directory) / "running_process-test.whl"
            with zipfile.ZipFile(wheel, "w") as archive:
                archive.writestr(expected, b"trampoline")

            self.assertEqual(build_wheel.verify_trampoline_in_wheel(wheel), expected)

    def test_rejects_a_wheel_without_the_trampoline(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            wheel = Path(directory) / "running_process-test.whl"
            with zipfile.ZipFile(wheel, "w") as archive:
                archive.writestr("running_process/assets/example.txt", b"example")

            with self.assertRaisesRegex(RuntimeError, "missing bundled trampoline"):
                build_wheel.verify_trampoline_in_wheel(wheel)


class BuildTrampolineTest(unittest.TestCase):
    def test_uses_a_separate_target_tree_from_maturin(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            executable = root / "built" / "daemon-trampoline.exe"
            executable.parent.mkdir(parents=True)
            executable.write_bytes(b"trampoline")
            completed = subprocess.CompletedProcess(
                args=["soldr", "cargo", "build"],
                returncode=0,
                stdout=json.dumps(
                    {
                        "reason": "compiler-artifact",
                        "target": {"name": "daemon-trampoline"},
                        "executable": str(executable),
                    }
                ),
                stderr="",
            )
            original_env = {"CARGO_TARGET_DIR": "shared-target"}
            with (
                patch.object(build_wheel, "ROOT", root),
                patch.object(
                    build_wheel,
                    "TRAMPOLINE_ASSETS",
                    root / "src" / "running_process" / "assets",
                ),
                patch.object(
                    build_wheel.subprocess,
                    "run",
                    return_value=completed,
                ) as run,
            ):
                self.assertEqual(
                    build_wheel.build_trampoline("dev", env=original_env), 0
                )

            child_env = run.call_args.kwargs["env"]
            self.assertEqual(
                child_env["CARGO_TARGET_DIR"], str(root / "target" / "trampoline")
            )
            self.assertEqual(original_env["CARGO_TARGET_DIR"], "shared-target")


class PreserveDevPdbTest(unittest.TestCase):
    def test_keeps_the_exact_wheel_build_artifact(self) -> None:
        triple = "x86_64-pc-windows-msvc"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "target" / triple / "debug" / "_native.pdb"
            source.parent.mkdir(parents=True)
            source.write_bytes(b"exact-codeview-identity")
            with (
                patch.object(build_wheel, "ROOT", root),
                patch("ci.env.host_target_triple", return_value=triple),
            ):
                preserved = build_wheel.preserve_dev_pdb()

            self.assertEqual(
                preserved, root / "target" / "probe-symbols" / triple / "_native.pdb"
            )
            self.assertEqual(preserved.read_bytes(), source.read_bytes())
