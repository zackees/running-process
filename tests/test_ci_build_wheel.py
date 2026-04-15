from __future__ import annotations

from ci import build_wheel


def test_linux_release_compatibility_args_pin_oldest_supported_glibc_floor() -> None:
    assert build_wheel.linux_release_compatibility_args() == [
        "--zig",
        "--compatibility",
        build_wheel.LINUX_GLIBC_COMPATIBILITY,
    ]
    assert build_wheel.LINUX_GLIBC_COMPATIBILITY == "manylinux2014"
    assert build_wheel.LINUX_GLIBC_MIN_VERSION == "2.17"


def test_build_command_linux_release_uses_manylinux2014(monkeypatch) -> None:
    monkeypatch.setattr(build_wheel.platform, "system", lambda: "Linux")
    monkeypatch.setattr(build_wheel.sys, "executable", "/tmp/python")

    command = build_wheel.build_command("release")

    assert command == [
        "/tmp/python",
        "-m",
        "maturin",
        "build",
        "--interpreter",
        "/tmp/python",
        "--out",
        str(build_wheel.DIST),
        "--release",
        "--zig",
        "--compatibility",
        "manylinux2014",
    ]


def test_build_command_linux_dev_does_not_force_manylinux(monkeypatch) -> None:
    monkeypatch.setattr(build_wheel.platform, "system", lambda: "Linux")
    monkeypatch.setattr(build_wheel.sys, "executable", "/tmp/python")

    command = build_wheel.build_command("dev")

    assert command == [
        "/tmp/python",
        "-m",
        "maturin",
        "build",
        "--interpreter",
        "/tmp/python",
        "--out",
        str(build_wheel.DIST),
        "--profile",
        "dev",
    ]


def test_build_command_non_linux_release_uses_pypi_compatibility(monkeypatch) -> None:
    monkeypatch.setattr(build_wheel.platform, "system", lambda: "Windows")
    monkeypatch.setattr(build_wheel.sys, "executable", "/tmp/python")

    command = build_wheel.build_command("release")

    assert command == [
        "/tmp/python",
        "-m",
        "maturin",
        "build",
        "--interpreter",
        "/tmp/python",
        "--out",
        str(build_wheel.DIST),
        "--release",
        "--compatibility",
        "pypi",
    ]
