import sys

import pytest

from running_process import ContainedProcessGroup


def test_contained_process_group_originator_spawn_and_close() -> None:
    group = ContainedProcessGroup("coverage-originator")
    assert group.originator == "coverage-originator"
    assert group.originator_value

    with pytest.raises(ValueError, match="argv must not be empty"):
        group.spawn([])
    with pytest.raises(ValueError, match="argv must not be empty"):
        group.spawn_daemon([])

    child_pid = group.spawn([sys.executable, "-c", "pass"])
    daemon_pid = group.spawn_daemon([sys.executable, "-c", "pass"])
    assert child_pid > 0
    assert daemon_pid > 0

    group.close()
    assert group.originator is None
    assert group.originator_value is None
    with pytest.raises(RuntimeError, match="group already closed"):
        group.spawn([sys.executable, "-c", "pass"])
    with pytest.raises(RuntimeError, match="group already closed"):
        group.spawn_daemon([sys.executable, "-c", "pass"])


def test_contained_process_group_context_manager_closes() -> None:
    group = ContainedProcessGroup()
    with group as entered:
        assert entered is group
        assert group.originator_value is None
    assert group.originator_value is None
    with pytest.raises(RuntimeError, match="group already closed"):
        group.spawn([sys.executable, "-c", "pass"])
