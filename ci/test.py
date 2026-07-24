from __future__ import annotations

import os
import shlex
import subprocess
import sys
from pathlib import Path

from ci.dev_build import ensure_dev_wheel
from ci.soldr import cargo_command

ROOT = Path(__file__).resolve().parent.parent
IN_RUNNING_PROCESS_ENV = "IN_RUNNING_PROCESS"
IN_RUNNING_PROCESS_VALUE = "running-process-cli"
GITHUB_ACTIONS_ENV = "GITHUB_ACTIONS"
SKIP_LINUX_DOCKER_ENV = "RUNNING_PROCESS_SKIP_LINUX_DOCKER"
DEFAULT_TEST_TIMEOUT_SECONDS = "40"
DEFAULT_COMMAND_TIMEOUT_SECONDS = 10.0
DEFAULT_RUST_TEST_TIMEOUT_SECONDS = 60.0
# Windows runs cargo nextest with --test-threads=1 for extra isolation around
# filesystem and named-pipe races. Serialized ConPTY teardown — Job Object close + child wait
# + reader-thread join — can stay quiet for 10s+ at a time, so the
# supervisor's idle window needs more headroom than the parallel POSIX path.
WINDOWS_RUST_TEST_TIMEOUT_SECONDS = 180.0
# The containerized run includes a silent maturin release build of the
# project wheel ("Building running-process @ file:///work"), which can stay
# quiet for ~3 minutes — give the idle watchdog the same headroom as the
# release-build phase.
DEFAULT_LINUX_TEST_TIMEOUT_SECONDS = 600.0
DEFAULT_RELEASE_BUILD_TIMEOUT_SECONDS = 600.0
DEFAULT_PYTEST_TIMEOUT_SECONDS = 40.0
COMMAND_TIMEOUT_ENV = "RUNNING_PROCESS_TEST_COMMAND_TIMEOUT_SECONDS"

# pytest-cov args for the first pytest run (creates fresh .coverage)
_COV_PYTEST_FIRST = [
    "--cov=running_process",
    "--cov-report=term",
]
# pytest-cov args for subsequent runs (appends, then writes final XML)
_COV_PYTEST_APPEND = [
    "--cov=running_process",
    "--cov-report=term",
    "--cov-report=xml:coverage-python.xml",
    "--cov-append",
]


def command_timeout_seconds() -> float | None:
    configured = os.environ.get(COMMAND_TIMEOUT_ENV)
    if configured is None:
        return DEFAULT_COMMAND_TIMEOUT_SECONDS
    configured = configured.strip()
    if not configured:
        return None
    timeout = float(configured)
    if timeout <= 0:
        return None
    return timeout


def supervised_command(
    python: Path,
    *command: str,
    timeout: float | None = None,
) -> list[str]:
    effective_timeout = command_timeout_seconds() if timeout is None else timeout
    if effective_timeout is None:
        return list(command)
    return [
        str(python),
        "-m",
        "running_process.cli",
        "--timeout",
        str(effective_timeout),
        "--",
        *command,
    ]


def _supervised_pytest_command(
    python: Path,
    *pytest_args: str,
) -> list[str]:
    # PTY-heavy Python suites can legitimately stay quiet for longer than the
    # default 10-second command timeout on loaded CI runners, especially under
    # coverage. Use the same wider window as the Linux docker path.
    return supervised_command(
        python,
        str(python),
        "-m",
        "pytest",
        "-vv",
        *pytest_args,
        timeout=DEFAULT_PYTEST_TIMEOUT_SECONDS,
    )


def _linux_unit_test_command(
    python: Path,
    *pytest_args: str,
) -> list[str]:
    command = [
        str(python),
        "-m",
        "ci.linux_docker",
        "all",
        "--output-dir",
        str(ROOT / "linux"),
    ]
    if pytest_args:
        command.extend(["--pytest-args", shlex.join(pytest_args)])
    return supervised_command(
        python,
        *command,
        timeout=DEFAULT_LINUX_TEST_TIMEOUT_SECONDS,
    )


def _release_build_command(python: Path) -> list[str]:
    return supervised_command(
        python,
        str(python),
        "build.py",
        "--release",
        timeout=DEFAULT_RELEASE_BUILD_TIMEOUT_SECONDS,
    )


def running_on_github_actions() -> bool:
    return os.environ.get(GITHUB_ACTIONS_ENV, "").lower() == "true"


def skip_linux_docker_preflight() -> bool:
    return os.environ.get(SKIP_LINUX_DOCKER_ENV, "").lower() in {
        "1",
        "true",
        "yes",
        "on",
    }


def run(cmd: list[str]) -> int:
    _, clean_env = load_env_helpers()
    return subprocess.run(cmd, cwd=ROOT, env=clean_env()).returncode


def _find_llvm_profdata() -> Path | None:
    """Locate the rustup toolchain's llvm-profdata (llvm-tools component)."""
    try:
        sysroot = subprocess.run(
            ["rustc", "--print", "sysroot"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return None
    exe = "llvm-profdata.exe" if sys.platform == "win32" else "llvm-profdata"
    for candidate in Path(sysroot).glob(f"lib/rustlib/*/bin/{exe}"):
        return candidate
    return None


def _prune_invalid_profraw(profile_dir: Path) -> None:
    """Delete .profraw files llvm-profdata cannot read on its own.

    Instrumented daemons this suite kills at teardown (SIGTERM/SIGKILL —
    e.g. the broker-v2 accept loop) never run the atexit profile flush,
    so a file caught mid-write is truncated. llvm-profdata crashes with
    SIGILL when such a file is in the merge set; validating each file
    individually and dropping the bad ones loses only the killed
    processes' counters while keeping the merge alive.
    """
    profdata = _find_llvm_profdata()
    if profdata is None or not profile_dir.is_dir():
        return
    pruned = 0
    for profraw in profile_dir.rglob("*.profraw"):
        probe = subprocess.run(
            [str(profdata), "show", str(profraw)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        if probe.returncode != 0:
            print(
                f"coverage: pruning invalid profraw ({probe.returncode}): {profraw}",
                flush=True,
            )
            profraw.unlink(missing_ok=True)
            pruned += 1
    print(f"coverage: pruned {pruned} invalid .profraw file(s)", flush=True)


def run_live(cmd: list[str]) -> int:
    _, clean_env = load_env_helpers()
    env = clean_env()
    env["RUNNING_PROCESS_LIVE_TESTS"] = "1"
    return subprocess.run(cmd, cwd=ROOT, env=env).returncode


def live_tests_enabled() -> bool:
    return os.environ.get("RUNNING_PROCESS_LIVE_TESTS") == "1"


def load_env_helpers():
    from ci.env import activate, clean_env

    return activate, clean_env


def _looks_like_pytest_target(arg: str) -> bool:
    return arg.endswith(".py") or "::" in arg or "/" in arg or "\\" in arg


def _normalize_pytest_args(args: list[str]) -> list[str]:
    if not args:
        return []
    if any(arg.startswith("-") for arg in args):
        return list(args)
    targets: list[str] = []
    selectors: list[str] = []
    collecting_targets = True
    for arg in args:
        if collecting_targets and _looks_like_pytest_target(arg):
            targets.append(arg)
            continue
        collecting_targets = False
        selectors.append(arg)
    normalized = list(targets or args[:1])
    if selectors:
        normalized.extend(["-k", " and ".join(selectors)])
    return normalized


def _pytest_exit_is_acceptable(returncode: int, pytest_args: list[str]) -> bool:
    if returncode == 0:
        return True
    return returncode == 5 and bool(pytest_args)


def _ensure_nextest_installed() -> bool:
    """Ensure `cargo nextest` is on PATH; install it on demand if not.

    Per-test timeouts and process isolation come from cargo-nextest
    plus `.config/nextest.toml`. CI workflows pre-install via
    `taiki-e/install-action`; this fallback covers local `./test` runs
    where the developer hasn't done so yet.
    """
    probe = subprocess.run(
        ["cargo", "nextest", "--version"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    if probe.returncode == 0:
        return True
    print(
        "cargo-nextest not found — installing (`cargo install cargo-nextest --locked`)…",
        flush=True,
    )
    install = subprocess.run(
        cargo_command("install", "cargo-nextest", "--locked"),
        cwd=ROOT,
    )
    if install.returncode != 0:
        print(
            "Failed to install cargo-nextest. Install it manually with:\n"
            "  cargo install cargo-nextest --locked\n"
            "or via the taiki-e/install-action GitHub Action.",
            file=sys.stderr,
            flush=True,
        )
        return False
    return True


def parse_args(argv: list[str] | None = None) -> tuple[list[str], bool, bool, bool]:
    argv = list(sys.argv[1:] if argv is None else argv)
    raw_pytest_args: list[str] = []
    require_symbols = False
    coverage = False
    live_only = False
    while argv:
        current = argv.pop(0)
        if current == "--no-skip":
            require_symbols = True
            continue
        if current == "--coverage":
            coverage = True
            continue
        if current == "--live-only":
            live_only = True
            continue
        raw_pytest_args.append(current)
    return _normalize_pytest_args(raw_pytest_args), require_symbols, coverage, live_only


def main(argv: list[str] | None = None) -> int:
    pytest_args, require_symbols, coverage, live_only = parse_args(argv)
    activate, _ = load_env_helpers()
    activate()
    if require_symbols:
        os.environ["RUNNING_PROCESS_REQUIRE_NATIVE_DEBUGGER_SYMBOLS"] = "1"
    if live_only:
        os.environ["RUNNING_PROCESS_LIVE_TESTS"] = "1"
    os.environ.setdefault("RUNNING_PROCESS_TEST_TIMEOUT_SECONDS", DEFAULT_TEST_TIMEOUT_SECONDS)
    python = Path(sys.executable)
    if os.environ.get(IN_RUNNING_PROCESS_ENV) != IN_RUNNING_PROCESS_VALUE:
        try:
            ensure_dev_wheel(python, root=ROOT)
        except RuntimeError as exc:
            print(str(exc), file=sys.stderr, flush=True)
            return 1

    # -- Rust tests (with optional coverage via cargo-llvm-cov) --
    #
    # We run via `cargo nextest run` rather than `cargo test`. Two wins:
    #
    # 1. Per-test PROCESS isolation — the pyo3 GIL + PTY deadlock that
    #    forces `--test-threads=1` under cargo test on Windows doesn't
    #    apply because each #[test] runs in its own process.
    # 2. Per-test WALL-CLOCK timeout from `.config/nextest.toml`
    #    (`slow-timeout.terminate-after`). Any test that hangs longer
    #    than the deadline is killed and its captured stdout/stderr
    #    appears in the nextest failure summary — enough for a CI agent
    #    to identify what hung and start fixing it.
    #
    # Build test binaries first WITHOUT the idle-timeout supervisor.
    # Compilation can have long gaps (>10s) with no stdout/stderr when
    # linking large crates (tokio, interprocess, clap, etc.) and the
    # 10-second idle-timeout would kill the process mid-compile.
    if not live_only:
        if not _ensure_nextest_installed():
            return 1

        if coverage:
            # Split run/report so corrupt .profraw files can be pruned in
            # between. This suite spawns instrumented daemons (the broker-v2
            # accept loop from #533 exits via SIGTERM in production) and
            # kills them at teardown; a process killed mid-profile-write
            # leaves a truncated .profraw and rustup's llvm-profdata
            # SIGILLs on it during the merge — coverage was red on every
            # run from the #533 merge (2026-06-21) onward. Dropping the
            # invalid files loses only the killed processes' counters.
            cargo_cmd = supervised_command(
                python,
                *cargo_command("llvm-cov", "nextest", "--workspace", "--no-report"),
                timeout=DEFAULT_RUST_TEST_TIMEOUT_SECONDS,
            )
            if run(cargo_cmd) != 0:
                return 1

            _prune_invalid_profraw(ROOT / "target" / "llvm-cov-target")

            report_cmd = cargo_command(
                "llvm-cov",
                "report",
                "--lcov",
                "--output-path",
                "coverage-rust.lcov",
            )
            if run(report_cmd) != 0:
                return 1
        else:
            # Step 1: compile all test binaries (no supervisor, no timeout)
            build_args = cargo_command("nextest", "run", "--workspace", "--no-run")
            if run(build_args) != 0:
                return 1

            # Step 2: run the pre-built tests under the idle-timeout supervisor.
            # nextest's per-test wall clock comes from .config/nextest.toml.
            cargo_test_args = cargo_command("nextest", "run", "--workspace")
            if sys.platform == "win32":
                # Belt-and-braces: even with process-per-test isolation,
                # filesystem and named-pipe races in the daemon test suite
                # are more reliable under serial execution on Windows.
                cargo_test_args += ["--test-threads", "1"]
            if os.environ.get("RUNNING_PROCESS_TEST_NOCAPTURE"):
                # CI-only: surface println!/eprintln! from Rust tests so
                # hangs and panics leave a usable trail in the GH log.
                cargo_test_args.append("--no-capture")
            rust_test_timeout = (
                WINDOWS_RUST_TEST_TIMEOUT_SECONDS
                if sys.platform == "win32"
                else DEFAULT_RUST_TEST_TIMEOUT_SECONDS
            )
            cargo_cmd = supervised_command(
                python,
                *cargo_test_args,
                timeout=rust_test_timeout,
            )
            if run(cargo_cmd) != 0:
                return 1

            # #433 R4: the RUNNING_PROCESS_FAKE_BACKEND seam is compiled out of
            # the default build (it must never ship in production). Exercise its
            # tests in a dedicated pass with the opt-in `test-seams` feature so
            # the backdoor stays covered without leaking into shipped binaries.
            seam_test_args = cargo_command(
                "nextest",
                "run",
                "-p",
                "running-process",
                "--features",
                "test-seams",
                "--test",
                "broker",
                "-E",
                "test(fake_backend)",
            )
            if sys.platform == "win32":
                seam_test_args += ["--test-threads", "1"]
            seam_cmd = supervised_command(
                python,
                *seam_test_args,
                timeout=rust_test_timeout,
            )
            if run(seam_cmd) != 0:
                return 1

        # -- Python non-live tests --
        cov_first = list(_COV_PYTEST_FIRST) if coverage else []
        if not _pytest_exit_is_acceptable(
            run(_supervised_pytest_command(python, "-m", "not live", *cov_first, *pytest_args)),
            pytest_args,
        ):
            return 1
        if not running_on_github_actions() and not skip_linux_docker_preflight():
            if run(_linux_unit_test_command(python, *pytest_args)) != 0:
                return 1
        if require_symbols and sys.platform == "win32":
            if run(_release_build_command(python)) != 0:
                return 1

    # -- Python live tests --
    if live_tests_enabled():
        cov_append = list(_COV_PYTEST_APPEND) if coverage else []
        if not _pytest_exit_is_acceptable(
            run_live(_supervised_pytest_command(python, "-m", "live", *cov_append, *pytest_args)),
            pytest_args,
        ):
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
