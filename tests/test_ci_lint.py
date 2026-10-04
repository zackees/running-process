from __future__ import annotations

from ci import lint as ci_lint

DYLINT_LANES = {"linux-x86", "macos-x86", "windows-x86"}


def test_native_x86_preflights_gate_both_workspace_dylints() -> None:
    workflows = ci_lint.ROOT / ".github" / "workflows"
    reusable = (workflows / "ci-preflight.yml").read_text(encoding="utf-8")
    assert "dylint --all --workspace" in reusable
    assert "--pattern running-process-platform-boundary" in reusable
    labels = {
        "ci-linux.yml": ("linux-x86", "linux-arm"),
        "ci-macos.yml": ("macos-x86", "macos-arm"),
        "ci-windows.yml": ("windows-x86", "windows-arm"),
    }
    for name, names in labels.items():
        source = (workflows / name).read_text(encoding="utf-8")
        for label in names:
            job = source.split(f"label: {label}", 1)[1].split("\n\n", 1)[0]
            assert ("dylint: true" in job) == (label in DYLINT_LANES), (name, label)


def test_dylint_job_uses_bash_so_windows_can_run_its_posix_steps() -> None:
    reusable = (ci_lint.ROOT / ".github" / "workflows" / "ci-preflight.yml").read_text(
        encoding="utf-8"
    )
    job = reusable.split("\n  dylint:\n", 1)[1].split("\n  lint-gates:\n", 1)[0]
    assert "defaults:\n      run:\n        shell: bash" in job
    # Windows binaries carry an .exe suffix; the cache path must match them.
    assert "cargo-dylint*" in job
    assert "dylint-link*" in job


def test_aggregate_check_fails_when_a_requested_dylint_did_not_pass() -> None:
    reusable = (ci_lint.ROOT / ".github" / "workflows" / "ci-preflight.yml").read_text(
        encoding="utf-8"
    )
    gate = reusable.split("\n  lint-gates:\n", 1)[1]
    assert "if: ${{ always() }}" in gate
    assert "needs: [preflight, dylint]" in gate
    assert '[ "$PREFLIGHT" = success ] || exit 1' in gate
    assert '[ "$DYLINT" = success ] || exit 1' in gate


def test_default_pr_path_does_not_run_macos_or_windows_dylint() -> None:
    main = (ci_lint.ROOT / ".github" / "workflows" / "ci.yml").read_text(
        encoding="utf-8"
    )
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
    reusable = (ci_lint.ROOT / ".github" / "workflows" / "ci-preflight.yml").read_text(
        encoding="utf-8"
    )
    assert dylint_gate.NIGHTLY in reusable
    assert f'cargo-dylint-version: "{dylint_gate.DYLINT_VERSION}"' in reusable


def test_dylint_gate_skips_without_toolchain_and_fails_when_required(
    monkeypatch, capsys
) -> None:
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
        lambda command, **kwargs: seen.append(command)
        or type("Result", (), {"returncode": 0})(),
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
