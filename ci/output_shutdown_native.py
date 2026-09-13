"""Run the native output-shutdown proof independently of unrelated PTY tests.

Uses the repository Cargo router and nextest's existing per-test deadlines.
The selected fixtures are self-executing unit tests; no testbins or wheel build
is needed. Every expected test must actually report PASS, including the Windows
queued-before-ReadFile race, rather than merely accepting a successful command.
"""

from __future__ import annotations

import platform
import re
import subprocess
import sys

from ci.soldr import cargo_command


def required_tests(host: str) -> dict[str, tuple[str, ...]]:
    session = "async_process::output_shutdown_tests::"
    native = (
        "output_shutdown_tests::silent_output_fixture",
        "output_shutdown_tests::shutdown_finishes_a_cancelled_pending_read_before_child_exit",
    )
    if host == "win32":
        native += (
            "platform_win::output_shutdown::tests::shutdown_retries_when_pipe_read_is_queued_before_its_syscall",
        )
    return {
        "running-process": tuple(
            session + name
            for name in (
                "helper",
                "cancelled_shutdown_observer_retries_and_broadcasts_only_after_cleanup",
                "descendant_held_pipe_shutdown_finishes_after_direct_child_reaping",
                "full_queue_shutdown_joins_pumps_without_consumer_or_child_exit",
                "pump_panic_still_enters_owned_reader_cleanup_before_reporting_failure",
                "silent_pending_read_shutdown_does_not_wait_for_direct_exit",
            )
        ),
        "running-process-platform-internal": native,
    }


def validate_passes(output: str, package: str, names: tuple[str, ...]) -> None:
    for name in names:
        pattern = r"^\s*(?:\d+\.\d+\s+)?PASS\s+\[[^\]]+\]\s+(?:\([^)]*\)\s+)?"
        pattern += re.escape(package) + r"\s+" + re.escape(name) + r"\s*$"
        if not re.search(pattern, output, flags=re.MULTILINE):
            raise ValueError(f"missing native PASS: {package} {name}")


def main() -> int:
    print(f"Native shutdown proof: {sys.platform} {platform.machine()}", flush=True)
    failed = False
    for package, names in required_tests(sys.platform).items():
        feature = (
            "kernel-substrate" if package == "running-process" else "async-process"
        )
        command = cargo_command(
            "nextest",
            "run",
            "--locked",
            "--package",
            package,
            "--no-default-features",
            "--features",
            feature,
            "--lib",
            "--build-jobs",
            "2",
            "--color",
            "never",
            "--retries",
            "0",
            "--no-fail-fast",
            "--status-level",
            "pass",
            "output_shutdown",
        )
        print(f"Running {command!r}", flush=True)
        result = subprocess.run(command, check=False, capture_output=True, text=True)
        output = result.stdout + result.stderr
        print(output, flush=True)
        if result.returncode:
            failed = True
            continue
        try:
            validate_passes(output, package, names)
        except ValueError as error:
            print(error, file=sys.stderr, flush=True)
            failed = True
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())
