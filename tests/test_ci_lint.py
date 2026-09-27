from __future__ import annotations

from ci import lint as ci_lint


def test_one_linux_preflight_gates_both_workspace_dylints() -> None:
    workflows = ci_lint.ROOT / ".github" / "workflows"
    reusable = (workflows / "ci-preflight.yml").read_text(encoding="utf-8")
    assert "dylint --all --workspace" in reusable
    assert "--pattern running-process-platform-boundary" in reusable
    for name in ("ci-linux.yml", "ci-macos.yml", "ci-windows.yml"):
        source = (workflows / name).read_text(encoding="utf-8")
        for label in ("linux-x86", "linux-arm") if name == "ci-linux.yml" else (
            ("macos-x86", "macos-arm") if name == "ci-macos.yml" else ("windows-x86", "windows-arm")
        ):
            job = source.split(f"label: {label}", 1)[1].split("\n\n", 1)[0]
            assert ("dylint: true" in job) == (label == "linux-x86"), (name, label)


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
    ]
