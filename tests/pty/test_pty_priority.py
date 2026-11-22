"""Nice value, priority enum, and posix_pty_command wrapping."""

from __future__ import annotations

import os
import sys
import tempfile
from pathlib import Path

import pytest

# `_pty_command` moved to the `_command` sub-module in the #151 refactor;
# patch its sub-module so the production lookup sees changes.
import running_process.pty._command as pty_module
from running_process import (
    CpuPriority,
    InteractiveMode,
    RunningProcess,
)
from tests.process_helpers import (
    WINDOWS_BELOW_NORMAL_PRIORITY_CLASS,
    windows_priority_class_script,
)
from tests.pty._pty_helpers import _read_until_contains

# POSIX nice tests only ever raise niceness: 19 (the maximum) for an int and
# CpuPriority.MINIMAL (15) for the enum. A lower value needs CAP_SYS_NICE when
# the test itself runs above it (a bosn replay on a host whose daemon runs
# SCHED_IDLE at nice 10), and the spawn fails with EACCES. Windows keeps
# nice=5 / CpuPriority.LOW (BELOW_NORMAL_PRIORITY_CLASS).
POSIX_NICE = 19
POSIX_PRIORITY = CpuPriority.MINIMAL
POSIX_PRIORITY_NICE = 15


def _require_posix_niceness_at_most(target: int) -> None:
    """Lowering niceness needs CAP_SYS_NICE. A test process that already runs
    nicer than `target` (a bosn replay whose daemon runs at nice 19) cannot
    exercise this request; hosted runners start at 0."""
    if sys.platform != "win32" and os.nice(0) > target:
        pytest.skip(
            f"this process already runs at nice {os.nice(0)}; "
            f"requesting {target} would need CAP_SYS_NICE"
        )


def test_pseudo_terminal_can_set_positive_nice() -> None:
    if sys.platform == "win32":
        process = RunningProcess.pseudo_terminal(
            [
                sys.executable,
                "-c",
                f"{windows_priority_class_script()}\nimport time\ntime.sleep(0.3)",
            ],
            text=True,
            nice=5,
        )
        output = _read_until_contains(process, str(WINDOWS_BELOW_NORMAL_PRIORITY_CLASS))
        assert str(WINDOWS_BELOW_NORMAL_PRIORITY_CLASS) in output
        assert process.wait(timeout=5) == 0
        return

    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import os, time; time.sleep(0.3); print(os.nice(0), flush=True)"],
        text=True,
        nice=POSIX_NICE,
    )
    output = _read_until_contains(process, str(POSIX_NICE))
    assert int(output.strip().splitlines()[-1]) == POSIX_NICE
    assert process.wait(timeout=5) == 0


def test_posix_pty_command_wraps_nice_before_exec(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(pty_module.sys, "platform", "darwin")

    command = pty_module._pty_command(["python", "-c", "print('x')"], False, 5)

    assert command[0] == sys.executable
    assert command[1:4] == [
        "-c",
        "import os, sys\n"
        "os.setpriority(os.PRIO_PROCESS, 0, int(sys.argv[1]))\n"
        "os.execvp(sys.argv[2], sys.argv[2:])\n",
        "5",
    ]
    assert command[4:] == ["python", "-c", "print('x')"]


def test_pseudo_terminal_accepts_priority_enum() -> None:
    _require_posix_niceness_at_most(POSIX_PRIORITY_NICE)
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import os, time; time.sleep(0.3); print(os.nice(0), flush=True)"]
        if sys.platform != "win32"
        else [
            sys.executable,
            "-c",
            f"{windows_priority_class_script()}\nimport time\ntime.sleep(0.3)",
        ],
        text=True,
        nice=CpuPriority.LOW if sys.platform == "win32" else POSIX_PRIORITY,
    )
    if sys.platform == "win32":
        output = _read_until_contains(process, str(WINDOWS_BELOW_NORMAL_PRIORITY_CLASS))
        assert str(WINDOWS_BELOW_NORMAL_PRIORITY_CLASS) in output
    else:
        output = _read_until_contains(process, str(POSIX_PRIORITY_NICE))
        assert int(output.strip().splitlines()[-1]) == POSIX_PRIORITY_NICE
    assert process.wait(timeout=5) == 0


def test_interactive_can_set_positive_nice() -> None:
    with tempfile.TemporaryDirectory() as temp_dir:
        output_path = Path(temp_dir) / "nice.txt"
        if sys.platform == "win32":
            script = windows_priority_class_script(output_path=output_path)
            expected = WINDOWS_BELOW_NORMAL_PRIORITY_CLASS
        else:
            script = (
                "from pathlib import Path\n"
                "import os\n"
                "import time\n"
                "time.sleep(0.3)\n"
                f"Path(r'{output_path}').write_text(str(os.nice(0)), encoding='utf-8')\n"
            )
            expected = POSIX_NICE

        process = RunningProcess.interactive(
            [sys.executable, "-c", script],
            mode=InteractiveMode.CONSOLE_SHARED,
            nice=5 if sys.platform == "win32" else POSIX_NICE,
        )
        assert process.wait(timeout=5) == 0
        observed = int(output_path.read_text(encoding="utf-8"))
        assert observed == expected


def test_interactive_accepts_priority_enum() -> None:
    _require_posix_niceness_at_most(POSIX_PRIORITY_NICE)
    with tempfile.TemporaryDirectory() as temp_dir:
        output_path = Path(temp_dir) / "nice.txt"
        if sys.platform == "win32":
            script = windows_priority_class_script(output_path=output_path)
            expected = WINDOWS_BELOW_NORMAL_PRIORITY_CLASS
        else:
            script = (
                "from pathlib import Path\n"
                "import os\n"
                "import time\n"
                "time.sleep(0.3)\n"
                "Path(r'"
                f"{output_path}"
                "').write_text(str(os.nice(0)), encoding='utf-8')\n"
            )
            expected = POSIX_PRIORITY_NICE

        process = RunningProcess.interactive(
            [sys.executable, "-c", script],
            mode=InteractiveMode.CONSOLE_SHARED,
            nice=CpuPriority.LOW if sys.platform == "win32" else POSIX_PRIORITY,
        )
        assert process.wait(timeout=5) == 0
        observed = int(output_path.read_text(encoding="utf-8"))
        assert observed == expected
