"""Retire the disabled all-features Rust target cache and enforce budget."""

from __future__ import annotations

import json
import os
import sys
import time
import urllib.error
import urllib.request
from collections.abc import Callable, Mapping
from typing import Any

API_ROOT = "https://api.github.com"
MAIN_REF = "refs/heads/main"
BUDGET_LIMIT_BYTES = 9_500_000_000
POLL_ATTEMPTS = 6
POLL_INTERVAL_SECONDS = 10
WRITER_BARRIER_ATTEMPTS = 356
WRITER_BARRIER_INTERVAL_SECONDS = 30
WRITER_QUIET_SAMPLES = 2
WRITER_QUIET_INTERVAL_SECONDS = 120
ACTIVE_RUN_STATUSES = ("queued", "in_progress", "waiting", "requested", "pending")
KNOWN_TERMINAL_RUN_STATUSES = {
    "completed",
}
# Display names of workflows that can write a durable Actions cache on main.
# Reusable workflows are included because GitHub exposes them in the Runs API
# separately on some event paths; unknown active statuses fail closed below.
CACHE_WRITER_WORKFLOWS = {
    "All-Features Tests (Windows/macOS)",
    "Auto Release",
    "Build Template",
    "CI Preflight (Linux)",
    "CI Preflight (macOS)",
    "CI Preflight (Windows)",
    "CI Preflight (Reusable)",
    "CI Preflight Rust-only (Reusable)",
    "Coverage",
    "Independent spawn",
    "Linux x86 Build",
    "Native output shutdown",
    "Linux ARM Build",
    "macOS ARM Build",
    "macOS x86 Build",
    "Security audit",
    "security-fuzz",
    "Windows ARM Build",
    "Windows x86 Build",
}
CACHE_WRITER_REUSABLE_WORKFLOWS = {
    "_build.yml",
    "ci-preflight-rust.yml",
    "ci-preflight.yml",
}
ALL_FEATURES_CACHE_PREFIXES = (
    "v0-rust-windows-x86-all-features-",
    "v0-rust-macos-x86-all-features-",
    "v0-rust-macos-arm-all-features-",
)


def is_retired_cache(cache: Mapping[str, Any]) -> bool:
    """Select only the three disabled cache families on main."""
    ref = cache.get("ref")
    key = cache.get("key")
    return (
        ref == MAIN_REF
        and isinstance(key, str)
        and key.startswith(ALL_FEATURES_CACHE_PREFIXES)
    )


def listed_bytes(caches: list[Mapping[str, Any]]) -> int:
    return sum(int(cache.get("size_in_bytes", 0)) for cache in caches)


def budget_bytes(endpoint_bytes: int, caches: list[Mapping[str, Any]]) -> int:
    """Use the conservative maximum while the usage endpoint/list may lag."""
    return max(endpoint_bytes, listed_bytes(caches))


def assert_main_sha(api: GitHubCacheAPI, expected_sha: str) -> None:
    current_sha = api.main_sha()
    if current_sha != expected_sha:
        raise RuntimeError(
            "main advanced during cache-budget workflow: "
            f"expected {expected_sha}, found {current_sha}"
        )


class GitHubCacheAPI:
    def __init__(
        self,
        repository: str,
        token: str,
        *,
        opener: Callable[..., Any] = urllib.request.urlopen,
    ) -> None:
        self.repository = repository
        self.token = token
        self.opener = opener

    def _request(self, path: str, *, method: str = "GET") -> Any:
        request = urllib.request.Request(
            f"{API_ROOT}/repos/{self.repository}{path}",
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {self.token}",
                "X-GitHub-Api-Version": "2022-11-28",
            },
            method=method,
        )
        try:
            with self.opener(request, timeout=30) as response:
                body = response.read()
        except urllib.error.HTTPError as error:
            if method == "DELETE" and error.code == 404:
                return None
            raise
        return json.loads(body) if body else None

    def list_caches(self) -> list[Mapping[str, Any]]:
        caches: list[Mapping[str, Any]] = []
        page = 1
        while True:
            payload = self._request(f"/actions/caches?per_page=100&page={page}")
            page_caches = payload.get("actions_caches", [])
            caches.extend(page_caches)
            if len(page_caches) < 100:
                return caches
            page += 1

    def usage_bytes(self) -> int:
        payload = self._request("/actions/cache/usage")
        return int(payload["active_caches_size_in_bytes"])

    def main_sha(self) -> str:
        payload = self._request("/git/ref/heads/main")
        return str(payload["object"]["sha"])

    def active_main_writer_runs(
        self, *, exclude_run_id: int
    ) -> list[Mapping[str, Any]]:
        runs: list[Mapping[str, Any]] = []
        for status in ACTIVE_RUN_STATUSES:
            page = 1
            while True:
                payload = self._request(
                    "/actions/runs?branch=main&per_page=100"
                    f"&status={status}&page={page}"
                )
                page_runs = payload.get("workflow_runs", [])
                for run in page_runs:
                    run_status = run.get("status")
                    if (
                        run_status
                        not in set(ACTIVE_RUN_STATUSES) | KNOWN_TERMINAL_RUN_STATUSES
                    ):
                        raise RuntimeError(
                            f"unknown workflow run status from GitHub API: {run_status!r}"
                        )
                    if (
                        int(run.get("id", -1)) != exclude_run_id
                        and run.get("head_branch") == "main"
                        and run.get("name") in CACHE_WRITER_WORKFLOWS
                        and run_status in ACTIVE_RUN_STATUSES
                    ):
                        runs.append(run)
                if len(page_runs) < 100:
                    break
                page += 1
        return runs

    def delete_cache(self, cache_id: int) -> None:
        self._request(f"/actions/caches/{cache_id}", method="DELETE")


def wait_for_main_writers(
    api: GitHubCacheAPI,
    *,
    expected_sha: str,
    own_run_id: int,
    sleep: Callable[[float], None] = time.sleep,
    attempts: int = WRITER_BARRIER_ATTEMPTS,
    interval: int = WRITER_BARRIER_INTERVAL_SECONDS,
    quiet_samples: int = WRITER_QUIET_SAMPLES,
    quiet_interval: int = WRITER_QUIET_INTERVAL_SECONDS,
) -> None:
    """Wait for main cache writers to finish and for their run records to settle."""
    consecutive_quiet = 0
    for attempt in range(1, attempts + 1):
        assert_main_sha(api, expected_sha)

        writers = api.active_main_writer_runs(exclude_run_id=own_run_id)
        if writers:
            consecutive_quiet = 0
            names = sorted({str(run.get("name", "unknown")) for run in writers})
            print(f"waiting for active main cache writers: {', '.join(names)}")
            if attempt < attempts:
                sleep(interval)
            continue

        consecutive_quiet += 1
        if consecutive_quiet >= quiet_samples:
            # Ensure no newly-dispatched push/scheduled writer appeared just
            # after the previous empty snapshot.
            return
        if attempt < attempts:
            sleep(quiet_interval)

    raise RuntimeError(
        f"main cache writers did not settle after {attempts} barrier snapshots"
    )


def enforce_budget(
    api: GitHubCacheAPI,
    *,
    expected_sha: str | None = None,
    sleep: Callable[[float], None] = time.sleep,
    attempts: int = POLL_ATTEMPTS,
    interval: int = POLL_INTERVAL_SECONDS,
) -> int:
    if expected_sha is not None:
        assert_main_sha(api, expected_sha)
    caches = api.list_caches()
    candidates = [cache for cache in caches if is_retired_cache(cache)]
    for cache in candidates:
        if expected_sha is not None:
            assert_main_sha(api, expected_sha)
        cache_id = int(cache["id"])
        print(
            "retiring disabled all-features cache "
            f"id={cache_id} ref={cache['ref']} size={cache['size_in_bytes']} "
            f"key={cache['key']}"
        )
        api.delete_cache(cache_id)

    previous_snapshot: tuple[int, int] | None = None
    for attempt in range(1, attempts + 1):
        if expected_sha is not None:
            assert_main_sha(api, expected_sha)
        endpoint_bytes = api.usage_bytes()
        current_caches = api.list_caches()
        listed_size = listed_bytes(current_caches)
        effective_size = budget_bytes(endpoint_bytes, current_caches)
        remaining = [cache for cache in current_caches if is_retired_cache(cache)]
        print(
            "cache budget: "
            f"max(endpoint={endpoint_bytes}, listed={listed_size})="
            f"{effective_size} bytes; limit={BUDGET_LIMIT_BYTES}; "
            f"retired={len(candidates)} entries/"
            f"{sum(int(c['size_in_bytes']) for c in candidates)} bytes; "
            f"remaining_disabled={len(remaining)}"
        )

        snapshot = (endpoint_bytes, listed_size)
        if not remaining and endpoint_bytes == listed_size:
            if snapshot == previous_snapshot:
                if effective_size > BUDGET_LIMIT_BYTES:
                    raise RuntimeError(
                        f"repository cache budget exceeded: {effective_size} > "
                        f"{BUDGET_LIMIT_BYTES} bytes"
                    )
                if expected_sha is not None:
                    assert_main_sha(api, expected_sha)
                return effective_size
            previous_snapshot = snapshot
        else:
            previous_snapshot = None

        if attempt < attempts:
            sleep(interval)

    raise RuntimeError(
        "cache inventory did not converge below budget within " f"{attempts} snapshots"
    )


def main() -> int:
    if os.environ.get("GITHUB_REF") != MAIN_REF:
        print("cache budget cleanup is restricted to refs/heads/main", file=sys.stderr)
        return 2
    repository = os.environ.get("GITHUB_REPOSITORY")
    token = os.environ.get("GITHUB_TOKEN")
    expected_sha = os.environ.get("GITHUB_SHA")
    run_id = os.environ.get("GITHUB_RUN_ID")
    if not repository or not token or not expected_sha or not run_id:
        print(
            "GITHUB_REPOSITORY, GITHUB_TOKEN, GITHUB_SHA, and GITHUB_RUN_ID are required",
            file=sys.stderr,
        )
        return 2

    api = GitHubCacheAPI(repository, token)
    try:
        wait_for_main_writers(
            api,
            expected_sha=expected_sha,
            own_run_id=int(run_id),
        )
        size = enforce_budget(api, expected_sha=expected_sha)
    except (RuntimeError, urllib.error.URLError, ValueError, KeyError) as error:
        print(f"cache budget enforcement failed: {error}", file=sys.stderr)
        return 1
    print(f"cache budget green: {size} <= {BUDGET_LIMIT_BYTES} bytes")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
