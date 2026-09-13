"""Run the native Windows independent-launch integration on a real host."""

from __future__ import annotations

import subprocess
import sys

from ci.soldr import cargo_command


def main() -> int:
    if sys.platform != "win32":
        raise SystemExit("This stage requires a real Windows Task Scheduler host")
    suites = [
        (
            ["-p", "running-process-platform-internal", "--lib"],
            ["nonblocking_large_frame_round_trip", "--nocapture"],
        ),
        (
            ["-p", "running-process-platform-internal", "--lib"],
            ["scheduler_launch::tests", "--include-ignored", "--nocapture"],
        ),
        (
            ["-p", "running-process", "--test", "independent_spawn_windows"],
            [
                "--exact",
                "scheduler_starts_and_stops_the_verified_target",
                "--ignored",
                "--nocapture",
            ],
        ),
    ]
    for package, test_filter in suites:
        command = cargo_command(
            "test", *package, "--no-default-features", "--features", "independent-spawn"
        )
        build = subprocess.run([*command, "--no-run"], check=False, timeout=900)
        if build.returncode:
            return build.returncode
        test = subprocess.run([*command, "--", *test_filter], check=False, timeout=120)
        if test.returncode:
            return test.returncode
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
