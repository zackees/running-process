"""Safety and artifact-selection checks for the disposable broker fixture."""

import json
import unittest
from pathlib import Path
from unittest.mock import mock_open, patch

from ci import independent_broker_docker as stage


@patch.object(stage.sys, "platform", "linux")
class DockerBrokerStageTests(unittest.TestCase):
    def test_native_fixture_rejects_wrong_architecture_and_non_elf(self):
        def header(machine):
            return b"\x7fELF\x02\x01" + bytes(12) + machine.to_bytes(2, "little")

        with patch.object(stage.platform, "machine", return_value="aarch64"):
            with patch.object(Path, "open", mock_open(read_data=header(183))):
                stage.verify_native_fixture(Path("/test"), Path("/launcher"))
            for invalid in [header(62), b"not an ELF"]:
                with patch.object(Path, "open", mock_open(read_data=invalid)):
                    with self.assertRaises(ValueError):
                        stage.verify_native_fixture(Path("/test"), Path("/launcher"))

    @patch.object(stage, "build_fixture", create=True)
    @patch.object(stage, "stage_fixture", create=True)
    @patch.object(stage.shutil, "which", return_value="soldr")
    def test_build_only_does_not_run_docker(self, which, stage_fixture, build):
        build.return_value = (Path("/built/test"), Path("/built/launcher"))
        with patch.object(stage, "run_container") as run:
            self.assertEqual(
                stage.main(
                    [
                        "--build-only",
                        "/output",
                        "--target",
                        "aarch64-unknown-linux-musl",
                    ]
                ),
                0,
            )
        run.assert_not_called()
        build.assert_called_once_with("aarch64-unknown-linux-musl")
        stage_fixture.assert_called_once_with(
            Path("/output"), Path("/built/test"), Path("/built/launcher")
        )

    @patch.object(stage, "verify_native_fixture", create=True)
    @patch.object(stage.shutil, "which", return_value="docker")
    def test_run_only_never_builds(self, which, verify):
        with (
            patch.object(stage, "build_fixture", create=True) as build,
            patch.object(stage, "run_container", return_value=0) as run,
        ):
            self.assertEqual(stage.main(["--run-only", "/fixture"]), 0)
        build.assert_not_called()
        expected = (
            Path("/fixture/test").resolve(),
            Path("/fixture/launcher").resolve(),
        )
        verify.assert_called_once_with(*expected)
        self.assertEqual(run.call_args.args[1:], expected)

    def test_native_target_matches_supported_runner_architecture(self):
        self.assertEqual(stage.native_target("x86_64"), "x86_64-unknown-linux-musl")
        self.assertEqual(stage.native_target("aarch64"), "aarch64-unknown-linux-musl")
        self.assertEqual(stage.native_target("arm64"), "aarch64-unknown-linux-musl")
        with self.assertRaises(ValueError):
            stage.native_target("riscv64")

    def test_artifacts_come_from_this_build_not_directory_globs(self):
        records = [
            {
                "reason": "compiler-artifact",
                "target": {"name": name},
                "executable": path,
            }
            for name, path in [
                ("independent_broker_docker", "/build/current-test"),
                ("running-process-launcher", "/build/current-launcher"),
            ]
        ]
        output = "build preamble\n" + "\n".join(map(json.dumps, records))
        self.assertEqual(
            stage.artifacts(output),
            (
                Path("/build/current-test").resolve(),
                Path("/build/current-launcher").resolve(),
            ),
        )
        with self.assertRaises(ValueError):
            stage.artifacts("no compiler artifacts")

    def test_container_has_private_limits_and_only_readonly_binary_mounts(self):
        command = stage.container_command(
            "fixture-owned", Path("/test"), Path("/launcher")
        )
        self.assertIn("--rm", command)
        for flag, value in [
            ("--name", "fixture-owned"),
            ("--network", "none"),
            ("--memory", "128m"),
            ("--memory-swap", "128m"),
            ("--cgroupns", "private"),
            ("--entrypoint", "/fixture/test"),
        ]:
            self.assertEqual(command[command.index(flag) + 1], value)
        mounts = [
            command[i + 1] for i, value in enumerate(command) if value == "--mount"
        ]
        self.assertEqual(len(mounts), 2)
        self.assertTrue(all(mount.endswith(",readonly") for mount in mounts))
        self.assertFalse(any("/sys/fs/cgroup" in mount for mount in mounts))
        self.assertEqual(
            command[-4:],
            ["--exact", "docker_broker_accounting", "--ignored", "--nocapture"],
        )

    @patch.object(stage.subprocess, "run")
    def test_failure_still_removes_only_the_owned_container(self, run):
        run.side_effect = [RuntimeError("run failed"), None]
        with self.assertRaisesRegex(RuntimeError, "run failed"):
            stage.run_container("fixture-owned", Path("/test"), Path("/launcher"))
        self.assertEqual(
            run.call_args_list[-1].args[0], ["docker", "rm", "--force", "fixture-owned"]
        )


if __name__ == "__main__":
    unittest.main()
