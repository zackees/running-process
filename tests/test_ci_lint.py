from __future__ import annotations

from ci import lint as ci_lint

WORKFLOWS = ci_lint.ROOT / ".github" / "workflows"
CROSS_TARGETS = {
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "aarch64-unknown-linux-gnu",
}


def test_lint_is_one_linux_job_running_the_host_lint_lane() -> None:
    """Lint never runs under bosn; soldr lint only. The reusable preflight the
    gate replays holds no lint; ci.yml's `lint` job runs the host lane, which
    cross-lints every platform the per-platform preflights used to lint."""
    import tomllib

    from ci import local_gate

    reusable = (WORKFLOWS / "ci-preflight.yml").read_text(encoding="utf-8")
    for word in ("dylint", "--module ci.lint", "lint-gates", "inputs.lint"):
        assert word not in reusable, word
    main = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    job = main.split("\n  lint:\n", 1)[1].split("\n\n  linux-full:\n", 1)[0]
    assert "runs-on: ubuntu-24.04" in job
    assert "python ci/local_gate.py --lane lint" in job
    assert "needs.ci-mode.outputs.skip_lint != 'true'" in job
    assert "uses: zackees/setup-soldr@v0" in job
    assert 'cargo-dylint-version: "6.2.0"' in job
    assert 'dylint-link-version: "6.2.0"' in job
    commands = [argv for argv, _ in local_gate.lint_commands()]
    assert commands[0] == ["soldr", "lint", "rust", "--check", "--cross-dylint"]
    assert any("running-process-platform-boundary" in argv for argv in commands)
    cargo = tomllib.loads((ci_lint.ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    assert set(cargo["workspace"]["metadata"]["soldr"]["targets"]) == CROSS_TARGETS
    for name in ("ci-linux.yml", "ci-macos.yml", "ci-windows.yml"):
        assert "dylint" not in (WORKFLOWS / name).read_text(encoding="utf-8"), name


def test_default_pr_path_does_not_run_macos_or_windows_dylint() -> None:
    main = (ci_lint.ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    jobs = main.split("\n\n")
    for name in ("ci-macos.yml", "ci-windows.yml"):
        job = next(j for j in jobs if f"uses: ./.github/workflows/{name}" in j)
        assert "    if:" in job, f"{name} must stay gated behind its opt-in tier"
        assert "schedule" in job
        assert "ci-mac" in job or "ci-windows" in job


def test_local_lint_runs_both_dylint_libraries_in_ci_order() -> None:
    from ci import dylint_gate

    first, second = dylint_gate.commands()
    assert first[-2:] == ["--all", "--workspace"]
    assert "running-process-platform-boundary" in second
    prefix = ["soldr", "dylint"]
    assert first[:2] == prefix
    assert second[:2] == prefix
    assert "--all" in second
    main = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    assert dylint_gate.NIGHTLY in main
    assert f'cargo-dylint-version: "{dylint_gate.DYLINT_VERSION}"' in main


def test_dylint_gate_skips_without_toolchain_and_fails_when_required(monkeypatch, capsys) -> None:
    from ci import dylint_gate

    monkeypatch.setattr(dylint_gate, "missing_tools", lambda: ["install-it"])
    monkeypatch.delenv(dylint_gate.REQUIRE_ENV, raising=False)
    assert dylint_gate.main() == 0
    assert "skipped" in capsys.readouterr().err
    monkeypatch.setenv(dylint_gate.REQUIRE_ENV, "1")
    assert dylint_gate.main() == 1
    assert "FAILED" in capsys.readouterr().err


def test_dylint_gate_runs_every_command_and_stops_at_the_first_violation(
    monkeypatch,
) -> None:
    from ci import dylint_gate

    monkeypatch.setattr(dylint_gate, "missing_tools", lambda: [])
    seen: list[list[str]] = []

    def fake_run(command, **kwargs):
        seen.append(command)
        assert kwargs["env"]["CARGO_TARGET_DIR"] == str(dylint_gate.TARGET_DIR)
        return type("Result", (), {"returncode": 1 if len(seen) == 1 else 0})()

    monkeypatch.setattr(dylint_gate.subprocess, "run", fake_run)
    assert dylint_gate.main() == 1
    assert seen == dylint_gate.commands()[:1]
    seen.clear()
    monkeypatch.setattr(
        dylint_gate.subprocess,
        "run",
        lambda command, **kwargs: seen.append(command) or type("Result", (), {"returncode": 0})(),
    )
    assert dylint_gate.main() == 0
    assert seen == dylint_gate.commands()


def test_main_runs_lint_commands_through_running_process_cli(monkeypatch) -> None:
    commands: list[list[str]] = []
    monkeypatch.setattr(
        ci_lint,
        "repo_python",
        lambda: ci_lint.ROOT / ".venv" / "Scripts" / "python.exe",
    )
    monkeypatch.setattr(ci_lint, "cargo_command", lambda *args: ["cargo", *args])
    monkeypatch.setattr(ci_lint, "load_env_helpers", lambda: (lambda: None, lambda: {}))
    monkeypatch.setattr(
        ci_lint,
        "run",
        lambda cmd: commands.append(list(cmd)) or 0,
    )

    result = ci_lint.main()

    python = str(ci_lint.ROOT / ".venv" / "Scripts" / "python.exe")
    timeout = str(ci_lint.DEFAULT_COMMAND_TIMEOUT_SECONDS)
    assert result == 0
    assert commands == [
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.version_check",
        ],
        # #1189 added an ABI guard here and did not extend this list, so this
        # test has been red since. Restored as `ci.wheel_abi_guard`: it keeps
        # every published wheel ABI3 so one wheel per platform covers 3.10+.
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.wheel_abi_guard",
        ],
        # #1216: no workflow cache step may save from a pull_request run.
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.cache_save_guard",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.spawn_path_guard",
        ],
        # soldr#1176 added this guard between the spawn-path guard and the
        # platform boundary: it resolves every name `.config/nextest.toml`
        # mentions against the test targets that exist, so a filter orphaned
        # by a rename fails lint instead of silently matching nothing.
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.nextest_filter_guard",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.platform_boundary",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.async_compliance_guard",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.minimal_async_platform_graph",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.kernel_substrate_contract",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.backend_identity_contract",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.frame_v1_codec_contract",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.daemon_registration_contract",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.daemon_registration_v2_contract",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.parity_manifest",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.api_snapshot",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.sync_test_audit",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.docker_manifest_guard",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.jemalloc_guard",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.cross_compiler_guard",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            "cargo",
            "fmt",
            "--all",
            # Verify, never rewrite: plain `cargo fmt --all` exits 0 whether or
            # not it changed anything, so it could never fail CI and it
            # silently rewrote contributors' working trees. See #694.
            "--",
            "--check",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            "cargo",
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ruff",
            "check",
            "--fix",
            "src",
            "tests",
            "ci",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.lint_python.keyboard_interrupt_checker",
            # Scoped to `src`: the rule is about library code, where an
            # interrupt on a worker thread has to reach the main thread to be
            # seen at all.
            "src",
            "--exclude",
            ".venv",
            "venv",
            "dist",
            ".build",
        ],
        [python, "-m", "ci.dylint_gate"],
    ]


def test_merge_queue_runs_cancel_superseded_first_and_ends_with_janitor() -> None:
    """GEN-022 (forge-ci/ci.yml#405/#409): the queue's housekeeping shape."""
    main = (ci_lint.ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    assert "  merge_group:\n    types: [checks_requested]\n" in main
    jobs = main.split("\n\n")
    cancel = next(j for j in jobs if "\n  cancel-superseded:\n" in j)
    assert "github.event_name == 'merge_group'" in cancel
    assert "\n    needs:" not in cancel
    assert "actions: write" in cancel
    assert "run: python3 -m ci.cancel_superseded" in cancel
    janitor = next(j for j in jobs if "\n  queue-cache-janitor:\n" in j)
    assert "always() && github.event_name == 'merge_group'" in janitor
    assert "group: cache-janitor\n      cancel-in-progress: false" in janitor
    assert "run: python3 -m ci_lint cache janitor --repo ." in janitor
    ids = {
        line.strip().rstrip(":")
        for line in main.split("\njobs:\n", 1)[1].splitlines()
        if line.startswith("  ") and not line.startswith("   ") and line.endswith(":")
    }
    needs = janitor.split("needs: [", 1)[1].split("]", 1)[0].split(", ")
    assert set(needs) == ids - {"queue-cache-janitor"}
    # Nothing else may wait on cancel-superseded.
    assert main.count("cancel-superseded") == janitor.count("cancel-superseded") + (
        cancel.count("cancel-superseded")
    )
