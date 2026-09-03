from __future__ import annotations

import unittest
from unittest import mock

from running_process import _abi_guard


class TestFreeThreadedAbiGuard(unittest.TestCase):
    """#1142: a free-threaded interpreter must not load a GIL-runtime build."""

    def _with_interpreter(self, *, gil_disabled: int, ext_suffix: str):
        values = {"Py_GIL_DISABLED": gil_disabled, "EXT_SUFFIX": ext_suffix}
        return mock.patch.object(
            _abi_guard.sysconfig, "get_config_var", values.__getitem__
        )

    def test_gil_enabled_interpreter_is_never_blocked(self) -> None:
        with self._with_interpreter(gil_disabled=0, ext_suffix=".cp313-win_amd64.pyd"):
            self.assertEqual(_abi_guard._mismatched_extensions(), [])

    def test_free_threaded_interpreter_rejects_the_abi3_extension(self) -> None:
        with (
            self._with_interpreter(gil_disabled=1, ext_suffix=".cp313t-win_amd64.pyd"),
            mock.patch.object(
                _abi_guard.Path,
                "glob",
                return_value=[_abi_guard.Path("_native.pyd")],
            ),
        ):
            self.assertEqual(_abi_guard._mismatched_extensions(), ["_native.pyd"])
            with self.assertRaisesRegex(ImportError, "free-threaded interpreter"):
                _abi_guard._check()

    def test_free_threaded_interpreter_accepts_its_own_extension(self) -> None:
        suffix = ".cp313t-win_amd64.pyd"
        with (
            self._with_interpreter(gil_disabled=1, ext_suffix=suffix),
            mock.patch.object(
                _abi_guard.Path,
                "glob",
                return_value=[_abi_guard.Path(f"_native{suffix}")],
            ),
        ):
            self.assertEqual(_abi_guard._mismatched_extensions(), [])
