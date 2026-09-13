"""Safety and artifact-selection checks for the disposable broker fixture."""

import json
import unittest
from pathlib import Path
from unittest.mock import patch

from ci import independent_broker_docker as stage


class DockerBrokerStageTests(unittest.TestCase):
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
            (Path("/build/current-test"), Path("/build/current-launcher")),
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
