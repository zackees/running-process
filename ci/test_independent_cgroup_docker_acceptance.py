"""Docker orchestration contracts without Docker or cgroup mutation."""
from pathlib import Path
import unittest
from unittest import mock
import subprocess

from ci.independent_cgroup_docker_acceptance import container_boundary, exec_argv, run


class DockerTopologyTests(unittest.TestCase):
    identity = "a" * 64

    def test_boundary_is_container_not_init_leaf_or_host_parent(self) -> None:
        outer = container_boundary(self.identity, 123, f"0::/docker/{self.identity}/init\n")
        self.assertEqual(outer, Path("/sys/fs/cgroup/docker") / self.identity)

    def test_unsupported_or_ambiguous_membership_is_refused(self) -> None:
        for membership in (
            "0::/unrelated\n", f"0::/docker/{self.identity}/../other\n",
            f"0::/docker/{self.identity}/{self.identity}\n",
            f"0::/system.slice/docker-{self.identity}.scope\n",
            f"1:memory:/docker/{self.identity}\n",
        ):
            with self.subTest(membership=membership), self.assertRaises(RuntimeError):
                container_boundary(self.identity, 123, membership)

    def test_short_container_identity_is_refused(self) -> None:
        with self.assertRaises(RuntimeError):
            container_boundary("abc123", 123, "0::/docker/abc123\n")

    def test_exec_preserves_profile_paths_and_uses_no_shell_or_lifecycle_mutation(self) -> None:
        outer = Path("/sys/fs/cgroup/docker") / self.identity
        command = exec_argv(
            ["docker", "--host", "unix:///var/run/docker.sock"], self.identity,
            Path("/source/ci/independent_cgroup_acceptance.py"),
            Path("/artifacts/debug/helper"), Path("/artifacts/debug/deps/test binary"),
            outer / "delegated", outer,
        )
        self.assertEqual(command[3:6], ["exec", self.identity, "python3"])
        self.assertEqual(command[-1], "/artifacts/debug/deps/test binary")
        self.assertIn("--container-cgroup", command)
        for forbidden in ("--privileged", "create", "stop", "rm", "sh", "--pid=host"):
            self.assertNotIn(forbidden, command)

    def test_relative_container_artifact_path_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            exec_argv(["docker"], self.identity, Path("runner.py"), Path("/helper"),
                      Path("/test"), Path("/group/child"), Path("/group"))


class DockerRunTests(unittest.TestCase):
    identity = "b" * 64

    def invoke(self, *, limits=None, epochs=None, execution_error=None) -> mock.Mock:
        outer = f"/sys/fs/cgroup/docker/{self.identity}"
        memory = iter(limits or ["1073741824", "1073741824"])
        records = [
            {"Id": self.identity, "State": {"Pid": 123, "StartedAt": epoch}}
            for epoch in (epochs or ["original", "original", "original"])
        ]

        def read(path: Path, *args, **kwargs) -> str:
            if str(path) == "/proc/123/cgroup":
                return f"0::/docker/{self.identity}/init\n"
            if str(path) == outer + "/memory.max":
                return next(memory)
            raise AssertionError(f"unexpected host read: {path}")

        with mock.patch("sys.argv", [
            "runner", "--container", "fixture", "--runner", "/source/runner.py",
            "--cgroup-parent", outer + "/delegated", "--helper", "/profile/helper",
            "--test-binary", "/profile/deps/test",
        ]), mock.patch.object(Path, "resolve", autospec=True, side_effect=lambda path, **kw: path), \
                mock.patch.object(Path, "is_socket", return_value=True), \
                mock.patch.object(Path, "read_text", autospec=True, side_effect=read), \
                mock.patch("ci.independent_cgroup_docker_acceptance.inspect_running", side_effect=records), \
                mock.patch("ci.independent_cgroup_docker_acceptance.subprocess.run", side_effect=execution_error) as execute:
            self.execute = execute
            run()
            return execute

    def test_success_executes_once_without_stopping_operator_container(self) -> None:
        execute = self.invoke()
        execute.assert_called_once()
        argv = execute.call_args.args[0]
        self.assertEqual(argv[3:6], ["exec", self.identity, "python3"])
        self.assertTrue(execute.call_args.kwargs["check"])

    def test_restart_before_exec_prevents_launch(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "restarted during topology"):
            self.invoke(epochs=["original", "replacement"])
        self.execute.assert_not_called()

    def test_restart_after_exec_invalidates_success(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "restarted during acceptance"):
            self.invoke(epochs=["original", "original", "replacement"])
        self.execute.assert_called_once()

    def test_memory_limit_change_invalidates_success(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "memory limit changed"):
            self.invoke(limits=["1073741824", "max"])

    def test_timeout_reports_unconfirmed_cleanup_without_container_teardown(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "remote cleanup is unconfirmed"):
            self.invoke(execution_error=subprocess.TimeoutExpired("docker", 900))
        self.execute.assert_called_once()
