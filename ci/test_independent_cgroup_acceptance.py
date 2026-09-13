"""Pure filesystem cleanup guards; no Docker or cgroup mutation required."""
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from ci.independent_cgroup_acceptance import (
    child_exec, group_is_empty, remove_empty_cgroup_children,
    require_container_boundary,
)


class ContainerBoundaryTests(unittest.TestCase):
    def check_boundary(
        self, parent: str = "/sys/fs/cgroup/container/delegated",
        membership: str = "0::/container/init\n", limit: str = "1073741824",
        pids: str = "1234\n",
    ) -> None:
        files = {
            "/proc/self/cgroup": membership,
            "/sys/fs/cgroup/container/init/cgroup.procs": pids,
            "/sys/fs/cgroup/container/memory.max": limit,
        }

        def read(path: Path, *args, **kwargs) -> str:
            return files[str(path)]

        with mock.patch.object(Path, "resolve", autospec=True, side_effect=lambda path, **kw: path), \
                mock.patch.object(Path, "read_text", autospec=True, side_effect=read), \
                mock.patch("os.getpid", return_value=1234):
            require_container_boundary(Path(parent), Path("/sys/fs/cgroup/container"))

    def test_finite_outer_contains_runner_and_delegated_parent(self) -> None:
        self.check_boundary()

    def test_delegation_cannot_escape_container_or_use_its_root(self) -> None:
        for parent in ("/sys/fs/cgroup/sibling", "/sys/fs/cgroup/container"):
            with self.subTest(parent=parent), self.assertRaises(RuntimeError):
                self.check_boundary(parent=parent)

    def test_runner_membership_must_be_inside_same_container(self) -> None:
        for membership in ("0::/sibling/init\n", "0::/../host\n", "2:memory:/legacy\n"):
            with self.subTest(membership=membership), self.assertRaises(RuntimeError):
                self.check_boundary(membership=membership)

    def test_unlimited_invalid_or_zero_outer_is_refused(self) -> None:
        for limit in ("max", "0", "-1", "invalid"):
            with self.subTest(limit=limit), self.assertRaises(RuntimeError):
                self.check_boundary(limit=limit)

    def test_mount_and_pid_namespace_mismatch_is_refused(self) -> None:
        with self.assertRaises(RuntimeError):
            self.check_boundary(pids="9876\n")


class ChildPlacementTests(unittest.TestCase):
    def test_placement_and_close_precede_literal_exec(self) -> None:
        operations = mock.Mock()
        arguments = ["/fixture path", "space argument", "", "$literal"]
        with mock.patch("sys.argv", ["runner", "--child", "99", *arguments]), \
                mock.patch("os.getpid", return_value=1234), \
                mock.patch("os.write", return_value=4) as write, \
                mock.patch("os.close") as close, \
                mock.patch("os.execv") as execute:
            operations.attach_mock(write, "write")
            operations.attach_mock(close, "close")
            operations.attach_mock(execute, "execute")
            child_exec()
            self.assertEqual(operations.mock_calls, [
                mock.call.write(99, b"1234"),
                mock.call.close(99),
                mock.call.execute(arguments[0], arguments),
            ])

    def test_missing_executable_never_touches_control(self) -> None:
        with mock.patch("sys.argv", ["runner", "--child", "99"]), \
                mock.patch("os.write") as write, mock.patch("os.execv") as execute:
            with self.assertRaises(ValueError):
                child_exec()
            write.assert_not_called()
            execute.assert_not_called()

    def test_failed_or_short_placement_never_executes(self) -> None:
        for result in (0, OSError("placement denied")):
            with self.subTest(result=type(result).__name__), \
                    mock.patch("sys.argv", ["runner", "--child", "99", "/fixture"]), \
                    mock.patch("os.write") as write, \
                    mock.patch("os.close") as close, \
                    mock.patch("os.execv") as execute:
                if isinstance(result, OSError):
                    write.side_effect = result
                else:
                    write.return_value = result
                with self.assertRaises(OSError):
                    child_exec()
                close.assert_called_once_with(99)
                execute.assert_not_called()


@unittest.skipUnless(os.name == "posix", "directory-relative cleanup is Unix-only")
class OwnedGroupCleanupTests(unittest.TestCase):
    def test_empty_state_requires_explicit_zero(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
            try:
                for content, expected in (("", False), ("frozen 0\n", False),
                                          ("populated 1\n", False), ("populated 0\n", True)):
                    (root / "cgroup.events").write_text(content)
                    self.assertEqual(group_is_empty(fd), expected)
            finally:
                os.close(fd)

    def test_empty_children_removed_but_control_files_retained(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "child" / "nested").mkdir(parents=True)
            (root / "cgroup.events").write_text("populated 0\n")
            fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
            try:
                remove_empty_cgroup_children(fd)
            finally:
                os.close(fd)
            self.assertFalse((root / "child").exists())
            self.assertEqual((root / "cgroup.events").read_text(), "populated 0\n")

    def test_directory_link_is_rejected_without_touching_target(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            owned, outside = root / "owned", root / "outside"
            owned.mkdir()
            outside.mkdir()
            (outside / "keep").mkdir()
            (owned / "link").symlink_to(outside, target_is_directory=True)
            fd = os.open(owned, os.O_RDONLY | os.O_DIRECTORY)
            try:
                with self.assertRaises(OSError):
                    remove_empty_cgroup_children(fd)
            finally:
                os.close(fd)
            self.assertTrue((outside / "keep").is_dir())
