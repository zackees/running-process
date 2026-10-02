"""Tests for the main-only all-features cache retirement policy."""

import io
import json
import re
import unittest
from collections.abc import Mapping
from pathlib import Path
from typing import Any
from unittest.mock import patch
from urllib.error import HTTPError

from ci import cache_budget


def entry(
    cache_id: int,
    key: str,
    *,
    ref: str = "refs/heads/main",
    size: int = 100,
) -> Mapping[str, Any]:
    return {
        "id": cache_id,
        "key": key,
        "ref": ref,
        "size_in_bytes": size,
    }


class CacheBudgetPolicyTests(unittest.TestCase):
    def test_selects_only_exact_disabled_families_on_main(self) -> None:
        candidates = [
            entry(
                1,
                "v0-rust-windows-x86-all-features-all-features-Windows-x64-digest",
            ),
            entry(
                2,
                "v0-rust-macos-x86-all-features-all-features-Darwin-x64-digest",
            ),
            entry(
                3,
                "v0-rust-macos-arm-all-features-all-features-Darwin-arm64-digest",
            ),
            entry(4, "v0-rust-linux-x86-all-features-all-features-Linux-x64-digest"),
            entry(5, "v0-rust-windows-x86-shared-preflight-Windows-x64-digest"),
            entry(
                6,
                "v0-rust-windows-x86-all-features-all-features-Windows-x64-digest",
                ref="refs/pull/1216/merge",
            ),
            entry(
                7,
                "v0-rust-windows-11-arm-release-build-Windows_NT-arm64-hash-version",
            ),
            entry(
                8,
                "v0-rust-macos-15-intel-release-build-Darwin-x64-hash-version",
            ),
            entry(
                9,
                "v0-rust-windows-2025-release-build-Windows_NT-x64-hash-version",
            ),
            entry(
                10,
                "v0-rust-ubuntu-24.04-release-build-Linux-x64-hash-version",
            ),
            entry(
                11,
                "v0-rust-ubuntu-24.04-arm-release-build-Linux-arm64-hash-version",
            ),
            entry(
                12,
                "v0-rust-macos-15-release-build-Darwin-arm64-hash-version",
            ),
            entry(
                13,
                "v0-rust-release-binaries-x86_64-unknown-linux-gnu-build-binaries-Linux-x64-hash-version",
            ),
            entry(
                14,
                "v0-rust-release-binaries-aarch64-unknown-linux-gnu-build-binaries-Linux-x64-hash-version",
            ),
            entry(
                15,
                "v0-rust-release-binaries-x86_64-apple-darwin-build-binaries-Linux-x64-hash-version",
            ),
            entry(
                16,
                "v0-rust-release-binaries-aarch64-apple-darwin-build-binaries-Linux-x64-hash-version",
            ),
            entry(
                17,
                "v0-rust-release-binaries-x86_64-pc-windows-msvc-build-binaries-Linux-x64-hash-version",
            ),
            entry(
                18,
                "v0-rust-release-binaries-aarch64-pc-windows-msvc-build-binaries-Linux-x64-hash-version",
            ),
            entry(
                19,
                "v0-rust-windows-11-arm-dev-build-Windows_NT-arm64-hash-version",
            ),
            entry(
                20,
                "v0-rust-release-binaries-x86_64-pc-windows-msvc-build-binaries-Linux-x64-hash-version",
                ref="refs/tags/v4.10.15",
            ),
            entry(
                21,
                "v0-rust-windows-arm-shared-preflight-Windows_NT-arm64-hash-version",
            ),
            entry(
                22,
                "v0-rust-ubuntu-24.04-coverage-coverage-Linux-x64-hash-version",
            ),
            entry(
                23,
                "v0-rust-windows-arm-registry-preflight-Windows_NT-arm64-hash-version",
            ),
            entry(
                24,
                "v0-rust-ubuntu-24.04-coverage-registry-coverage-Linux-x64-hash-version",
            ),
        ]

        self.assertEqual(
            [
                int(cache["id"])
                for cache in candidates
                if cache_budget.is_retired_cache(cache)
            ],
            list(range(1, 4)) + list(range(7, 19)) + [21, 22],
        )

    def test_high_volume_lanes_keep_registry_only_under_new_namespaces(self) -> None:
        root = Path(__file__).resolve().parents[1]
        windows = (root / ".github/workflows/ci-windows.yml").read_text(
            encoding="utf-8"
        )
        arm_job = re.search(r"(?ms)^  arm:.*?(?=^  \w+:|\Z)", windows)
        self.assertIsNotNone(arm_job)
        self.assertIn("cache-key-suffix: registry", arm_job.group())

        preflight = (root / ".github/workflows/ci-preflight.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            "cache-key-suffix: ${{ inputs.label }}-${{ inputs.cache-key-suffix }}",
            preflight,
        )

        coverage = (root / ".github/workflows/coverage.yml").read_text(encoding="utf-8")
        coverage_cache = next(
            block
            for block in coverage.split("\n      - ")
            if "uses: zackees/setup-soldr@v0" in block
        )
        self.assertIn("cache-key-suffix: ubuntu-24.04-coverage", coverage_cache)
        self.assertIn(
            "save-cache: ${{ github.ref == 'refs/heads/main' && 'auto' || 'false' }}",
            coverage_cache,
        )
        self.assertTrue(
            {
                "v0-rust-windows-arm-shared-preflight-",
                "v0-rust-ubuntu-24.04-coverage-coverage-",
            }
            <= set(cache_budget.SUPERSEDED_TARGET_CACHE_PREFIXES)
        )
        self.assertFalse(
            any(
                prefix.endswith("registry-preflight-")
                or prefix.endswith("registry-coverage-")
                for prefix in cache_budget.RETIRED_CACHE_PREFIXES
            )
        )

    def test_release_target_caches_are_restore_only(self) -> None:
        root = Path(__file__).resolve().parents[1]
        build_workflow = (root / ".github/workflows/_build.yml").read_text(
            encoding="utf-8"
        )
        release_cache_step = next(
            block
            for block in build_workflow.split("\n      - ")
            if "uses: zackees/setup-soldr@v0" in block
        )
        self.assertIn(
            "save-cache: ${{ github.ref == 'refs/heads/main' && "
            "inputs['build-mode'] != 'release' && 'auto' || 'false' }}",
            release_cache_step,
        )

        release_workflow = (root / ".github/workflows/auto-release.yml").read_text(
            encoding="utf-8"
        )
        binary_cache_step = next(
            block
            for block in release_workflow.split("\n      - ")
            if "cache-key-suffix: release-binaries-${{ matrix.target }}" in block
        )
        self.assertIn('save-cache: "false"', binary_cache_step)

    def test_release_cache_tradeoff_is_documented(self) -> None:
        doc = (
            Path(__file__).resolve().parents[1] / "docs" / "cache-budget.md"
        ).read_text(encoding="utf-8")
        self.assertIn("release", doc.lower())
        self.assertIn("cold", doc.lower())
        self.assertIn("4,078,226,358", doc)

    def test_release_retention_prefixes_cover_workflow_targets(self) -> None:
        workflows = Path(__file__).resolve().parents[1] / ".github/workflows"
        release_workflow = (workflows / "auto-release.yml").read_text(encoding="utf-8")

        release_build_labels: set[str] = set()
        job_blocks = re.split(r"(?=^  [\w-]+:)", release_workflow, flags=re.MULTILINE)
        for job in job_blocks:
            if (
                "uses: ./.github/workflows/_build.yml" not in job
                or "build-mode: release" not in job
            ):
                continue
            runner = re.search(r"^\s+runs-on:\s*([^\s]+)", job, re.MULTILINE)
            self.assertIsNotNone(runner, "release build callsite needs runs-on")
            release_build_labels.add(runner.group(1))

        retired_release_build_labels = {
            prefix.removeprefix("v0-rust-").removesuffix("-release-build-")
            for prefix in cache_budget.RELEASE_BUILD_CACHE_PREFIXES
        }
        self.assertEqual(retired_release_build_labels, release_build_labels)

        build_binaries = re.search(
            r"(?ms)^  build-binaries:.*?(?=^  [\w-]+:|\Z)", release_workflow
        )
        self.assertIsNotNone(build_binaries)
        targets = set(
            re.findall(
                r"^\s+- target:\s*([^\s]+)",
                build_binaries.group(),
                re.MULTILINE,
            )
        )
        retired_targets = {
            prefix.removeprefix("v0-rust-release-binaries-").removesuffix(
                "-build-binaries-"
            )
            for prefix in cache_budget.RELEASE_BINARY_CACHE_PREFIXES
        }
        self.assertEqual(retired_targets, targets)

    def test_budget_uses_max_of_usage_endpoint_and_listing(self) -> None:
        caches = [entry(1, "v0-rust-current", size=200)]

        self.assertEqual(cache_budget.budget_bytes(300, caches), 300)
        self.assertEqual(cache_budget.budget_bytes(100, caches), 200)

    def test_budget_limit_is_conservative_decimal_target(self) -> None:
        self.assertEqual(cache_budget.BUDGET_LIMIT_BYTES, 9_500_000_000)

    def test_restore_only_all_features_cache_step(self) -> None:
        workflow = (
            Path(__file__).resolve().parents[1]
            / ".github"
            / "workflows"
            / "ci-all-features.yml"
        )
        lines = workflow.read_text(encoding="utf-8").splitlines()
        cache_step_start = next(
            index
            for index, line in enumerate(lines)
            if "uses: zackees/setup-soldr@v0" in line
        )
        step = "\n".join(lines[cache_step_start : cache_step_start + 8])
        self.assertIn('save-cache: "false"', step)
        self.assertIn("cache-key-suffix: ${{ matrix.label }}-all-features", step)

    def test_enforcer_retires_only_the_disabled_main_families(self) -> None:
        stale = [
            entry(
                1,
                "v0-rust-windows-x86-all-features-all-features-Windows-x64-old",
                size=800,
            ),
            entry(
                2,
                "v0-rust-macos-x86-all-features-all-features-Darwin-x64-old",
                size=600,
            ),
            entry(
                3,
                "v0-rust-macos-arm-all-features-all-features-Darwin-arm64-old",
                size=600,
            ),
            entry(4, "v0-rust-windows-x86-shared-preflight-Windows-x64", size=200),
            entry(
                5,
                "v0-rust-windows-11-arm-release-build-Windows_NT-arm64-hash-version",
                size=400,
            ),
            entry(
                6,
                "v0-rust-release-binaries-aarch64-pc-windows-msvc-build-binaries-Linux-x64-hash-version",
                size=500,
            ),
            entry(
                7,
                "v0-rust-windows-arm-shared-preflight-Windows_NT-arm64-hash-version",
                size=2100,
            ),
            entry(
                8,
                "v0-rust-ubuntu-24.04-coverage-coverage-Linux-x64-hash-version",
                size=900,
            ),
            entry(
                9,
                "v0-rust-windows-arm-registry-preflight-Windows_NT-arm64-hash-version",
                size=50,
            ),
            entry(
                10,
                "v0-rust-ubuntu-24.04-coverage-registry-coverage-Linux-x64-hash-version",
                size=50,
            ),
        ]

        class FakeAPI:
            def __init__(self) -> None:
                self.caches = list(stale)
                self.deleted: list[int] = []

            def list_caches(self) -> list[Mapping[str, Any]]:
                return list(self.caches)

            def usage_bytes(self) -> int:
                return cache_budget.listed_bytes(self.caches)

            def delete_cache(self, cache_id: int) -> None:
                self.deleted.append(cache_id)
                self.caches = [
                    cache for cache in self.caches if int(cache["id"]) != cache_id
                ]

        api = FakeAPI()
        budget = cache_budget.enforce_budget(
            api, sleep=lambda _seconds: None, attempts=2
        )

        self.assertEqual(api.deleted, [1, 2, 3, 5, 6, 7, 8])
        self.assertEqual([int(cache["id"]) for cache in api.caches], [4, 9, 10])
        self.assertEqual(budget, 300)

    def test_repeated_retirement_catches_a_cache_recreated_by_an_active_writer(
        self,
    ) -> None:
        class FakeAPI:
            def __init__(self) -> None:
                self.caches: list[Mapping[str, Any]] = [
                    entry(1, "v0-rust-macos-arm-all-features-all-features-old")
                ]
                self.deleted: list[int] = []

            def list_caches(self) -> list[Mapping[str, Any]]:
                return list(self.caches)

            def delete_cache(self, cache_id: int) -> None:
                self.deleted.append(cache_id)
                self.caches = [
                    cache for cache in self.caches if int(cache["id"]) != cache_id
                ]

        api = FakeAPI()
        first_phase = cache_budget.retire_disabled_caches(api)
        self.assertEqual([int(cache["id"]) for cache in first_phase], [1])

        # A scheduled run that started before the barrier can repopulate the
        # same retired family after the early pass; final enforcement catches it.
        api.caches.append(
            entry(2, "v0-rust-macos-arm-all-features-all-features-recreated")
        )
        second_phase = cache_budget.retire_disabled_caches(api)

        self.assertEqual([int(cache["id"]) for cache in second_phase], [2])
        self.assertEqual(api.deleted, [1, 2])
        self.assertEqual(api.caches, [])

    def test_enforcer_fails_when_other_families_keep_budget_over_limit(self) -> None:
        class FakeAPI:
            def __init__(self) -> None:
                self.caches = [
                    entry(
                        1,
                        "v0-rust-macos-arm-all-features-all-features-Darwin-arm64-old",
                        size=100,
                    ),
                    entry(
                        2,
                        "v0-rust-windows-x86-shared-preflight-Windows-x64",
                        size=cache_budget.BUDGET_LIMIT_BYTES + 1,
                    ),
                ]

            def list_caches(self) -> list[Mapping[str, Any]]:
                return list(self.caches)

            def usage_bytes(self) -> int:
                return cache_budget.listed_bytes(self.caches)

            def delete_cache(self, cache_id: int) -> None:
                self.caches = [
                    cache for cache in self.caches if int(cache["id"]) != cache_id
                ]

        with self.assertRaisesRegex(RuntimeError, "repository cache budget exceeded"):
            cache_budget.enforce_budget(
                FakeAPI(), sleep=lambda _seconds: None, attempts=2
            )

    def test_enforcer_waits_for_usage_and_listing_to_converge(self) -> None:
        class FakeAPI:
            def __init__(self) -> None:
                self.list_calls = 0

            def list_caches(self) -> list[Mapping[str, Any]]:
                self.list_calls += 1
                if self.list_calls == 1:
                    return []
                size = (
                    cache_budget.BUDGET_LIMIT_BYTES + 1
                    if self.list_calls < 4
                    else cache_budget.BUDGET_LIMIT_BYTES - 1
                )
                return [entry(9, "v0-rust-current", size=size)]

            def usage_bytes(self) -> int:
                return cache_budget.BUDGET_LIMIT_BYTES - 1

            def delete_cache(self, cache_id: int) -> None:
                raise AssertionError(f"unexpected deletion: {cache_id}")

        waits: list[float] = []
        result = cache_budget.enforce_budget(FakeAPI(), sleep=waits.append, attempts=6)

        self.assertEqual(result, cache_budget.BUDGET_LIMIT_BYTES - 1)
        self.assertEqual(waits, [10, 10, 10])

    def test_budget_workflow_is_nightly_only_and_permission_scoped(self) -> None:
        workflow = (
            Path(__file__).resolve().parents[1]
            / ".github"
            / "workflows"
            / "cache-budget.yml"
        )
        text = workflow.read_text(encoding="utf-8")
        self.assertIn("on:\n  workflow_call", text)
        self.assertNotIn("push:", text)
        self.assertIn(
            "permissions:\n  contents: read\n\njobs:\n  enforce-cache-budget:",
            text,
        )
        self.assertIn("permissions:\n      actions: write\n      contents: read", text)
        self.assertIn("run: uv run --no-project ci/cache_budget.py", text)
        self.assertIn("GITHUB_REF: ${{ github.ref }}", text)
        self.assertIn("GITHUB_SHA: ${{ github.sha }}", text)
        self.assertIn("GITHUB_RUN_ID: ${{ github.run_id }}", text)
        ci = (
            Path(__file__).resolve().parents[1] / ".github" / "workflows" / "ci.yml"
        ).read_text(encoding="utf-8")
        self.assertIn("needs: [linux-full, macos, windows, coverage, all-features]", ci)
        self.assertIn("!cancelled() && github.event_name == 'schedule'", ci)
        self.assertIn("uses: ./.github/workflows/cache-budget.yml", ci)
        self.assertNotIn("github.event_name == 'push'", ci)

    def test_budget_workflow_knows_the_all_features_writer_name(self) -> None:
        self.assertIn(
            "All-Features Tests (Windows/macOS)",
            cache_budget.CACHE_WRITER_WORKFLOWS,
        )
        self.assertNotIn("Cache budget (nightly)", cache_budget.CACHE_WRITER_WORKFLOWS)

    def test_writer_barrier_leaves_runner_timeout_headroom(self) -> None:
        workflow = (
            Path(__file__).resolve().parents[1]
            / ".github"
            / "workflows"
            / "cache-budget.yml"
        )
        text = workflow.read_text(encoding="utf-8")
        timeout = re.search(r"^    timeout-minutes:\s*(\d+)$", text, re.MULTILINE)
        self.assertIsNotNone(timeout)
        job_timeout_seconds = int(timeout.group(1)) * 60
        max_barrier_seconds = (
            (cache_budget.WRITER_BARRIER_ATTEMPTS - 1)
            * cache_budget.WRITER_BARRIER_INTERVAL_SECONDS
            + cache_budget.WRITER_QUIET_INTERVAL_SECONDS
        )
        self.assertLess(max_barrier_seconds, job_timeout_seconds - 60 * 60)

    def test_known_writer_inventory_covers_every_cache_writer_workflow(self) -> None:
        workflows = Path(__file__).resolve().parents[1] / ".github" / "workflows"
        discovered: set[str] = set()
        for workflow in workflows.glob("*.yml"):
            text = workflow.read_text(encoding="utf-8")
            direct_writer = any(
                marker in text
                for marker in (
                    "uses: zackees/setup-soldr@",
                    "uses: actions/cache/save@",
                    "uses: actions/cache@",
                )
            )
            reusable_writer = any(
                f"uses: ./.github/workflows/{template}" in text
                for template in cache_budget.CACHE_WRITER_REUSABLE_WORKFLOWS
            )
            if not direct_writer and not reusable_writer:
                continue
            match = re.search(r"^name:\s*(.+)$", text, re.MULTILINE)
            self.assertIsNotNone(match, f"cache writer needs workflow name: {workflow}")
            discovered.add(match.group(1).strip().strip("'\""))

        self.assertEqual(discovered, cache_budget.CACHE_WRITER_WORKFLOWS)
        for template in cache_budget.CACHE_WRITER_REUSABLE_WORKFLOWS:
            text = (workflows / template).read_text(encoding="utf-8")
            self.assertTrue(
                any(
                    marker in text
                    for marker in (
                        "uses: zackees/setup-soldr@",
                        "uses: actions/cache/save@",
                        "uses: actions/cache@",
                    )
                ),
                f"listed reusable cache writer has no cache writer step: {template}",
            )

    def test_writer_barrier_waits_for_active_writer_then_quiet_window(self) -> None:
        writer = {"id": 7, "name": "Coverage", "status": "in_progress"}

        class FakeAPI:
            def __init__(self) -> None:
                self.active_calls = 0
                self.sha_calls = 0

            def main_sha(self) -> str:
                self.sha_calls += 1
                return "abc123"

            def active_main_writer_runs(
                self, *, exclude_run_id: int
            ) -> list[Mapping[str, Any]]:
                self.assert_run_id = exclude_run_id
                self.active_calls += 1
                return [writer] if self.active_calls == 1 else []

        api = FakeAPI()
        waits: list[float] = []
        cache_budget.wait_for_main_writers(
            api,
            expected_sha="abc123",
            own_run_id=99,
            sleep=waits.append,
            attempts=4,
        )

        self.assertEqual(api.sha_calls, 3)
        self.assertEqual(api.active_calls, 3)
        self.assertEqual(api.assert_run_id, 99)
        self.assertEqual(
            waits,
            [
                cache_budget.WRITER_BARRIER_INTERVAL_SECONDS,
                cache_budget.WRITER_QUIET_INTERVAL_SECONDS,
            ],
        )

    def test_writer_barrier_fails_closed_if_main_advanced(self) -> None:
        class FakeAPI:
            def main_sha(self) -> str:
                return "newer-sha"

            def active_main_writer_runs(
                self, *, exclude_run_id: int
            ) -> list[Mapping[str, Any]]:
                raise AssertionError("must check the main SHA before querying runs")

        with self.assertRaisesRegex(RuntimeError, "main advanced"):
            cache_budget.wait_for_main_writers(
                FakeAPI(), expected_sha="old-sha", own_run_id=99, sleep=lambda _: None
            )

    def test_budget_rechecks_main_sha_before_each_delete(self) -> None:
        candidate = entry(
            1,
            "v0-rust-macos-arm-all-features-all-features-Darwin-arm64-old",
            size=100,
        )

        class FakeAPI:
            def __init__(self) -> None:
                self.sha_calls = 0
                self.deleted: list[int] = []

            def main_sha(self) -> str:
                self.sha_calls += 1
                return "expected" if self.sha_calls == 1 else "advanced"

            def list_caches(self) -> list[Mapping[str, Any]]:
                return [candidate]

            def delete_cache(self, cache_id: int) -> None:
                self.deleted.append(cache_id)

        api = FakeAPI()
        with self.assertRaisesRegex(RuntimeError, "main advanced"):
            cache_budget.enforce_budget(api, expected_sha="expected")

        self.assertEqual(api.deleted, [])

    def test_api_writer_inventory_fails_closed_on_unknown_status(self) -> None:
        class Response:
            def __enter__(self) -> "Response":
                return self

            def __exit__(self, *_args: object) -> None:
                return None

            def read(self) -> bytes:
                return json.dumps(
                    {
                        "workflow_runs": [
                            {
                                "id": 1,
                                "name": "Coverage",
                                "head_branch": "main",
                                "status": "mysterious",
                            }
                        ]
                    }
                ).encode()

        api = cache_budget.GitHubCacheAPI(
            "o/r", "token", opener=lambda *_args, **_kwargs: Response()
        )
        with self.assertRaisesRegex(RuntimeError, "unknown workflow run status"):
            api.active_main_writer_runs(exclude_run_id=99)

    def test_main_waits_for_writers_before_listing_or_deleting(self) -> None:
        calls: list[str] = []

        class FakeAPI:
            def __init__(self, *_args: object) -> None:
                pass

        with (
            patch.dict(
                "os.environ",
                {
                    "GITHUB_REF": "refs/heads/main",
                    "GITHUB_REPOSITORY": "o/r",
                    "GITHUB_TOKEN": "token",
                    "GITHUB_SHA": "abc123",
                    "GITHUB_RUN_ID": "99",
                },
                clear=True,
            ),
            patch.object(cache_budget, "GitHubCacheAPI", FakeAPI),
            patch.object(
                cache_budget,
                "retire_disabled_caches",
                side_effect=lambda _api, **kwargs: (
                    calls.append("preclean")
                    if kwargs == {"expected_sha": "abc123"}
                    else self.fail(f"unexpected preclean args: {kwargs}")
                ),
            ),
            patch.object(
                cache_budget,
                "wait_for_main_writers",
                side_effect=lambda *_args, **_kwargs: calls.append("barrier"),
            ),
            patch.object(
                cache_budget,
                "enforce_budget",
                side_effect=lambda _api, **kwargs: (
                    calls.append("enforce")
                    if kwargs == {"expected_sha": "abc123"}
                    else self.fail(f"unexpected enforcement args: {kwargs}")
                )
                or 0,
            ),
        ):
            self.assertEqual(cache_budget.main(), 0)

        self.assertEqual(calls, ["preclean", "barrier", "enforce"])

    def test_delete_404_is_idempotent_for_overlapping_main_runs(self) -> None:
        def not_found(_request: Any, *, timeout: int) -> Any:
            del timeout
            raise HTTPError(
                "https://api.github.com/repos/o/r/actions/caches/9",
                404,
                "not found",
                hdrs=None,
                fp=io.BytesIO(),
            )

        api = cache_budget.GitHubCacheAPI("o/r", "token", opener=not_found)
        self.assertIsNone(api.delete_cache(9))


if __name__ == "__main__":
    unittest.main()
