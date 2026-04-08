#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# ///
from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

import tomllib

ROOT = Path(__file__).resolve().parent.parent
DIST_DIR = ROOT / "dist"
WORKFLOWS = {
    "linux-x86.yml": "wheels-linux-x86",
    "linux-arm.yml": "wheels-linux-arm",
    "windows-x86.yml": "wheels-windows-x86",
    "windows-arm.yml": "wheels-windows-arm",
    "macos-x86.yml": "wheels-macos-x86",
    "macos-arm.yml": "wheels-macos-arm",
}


def log(msg: str) -> None:
    print(msg, file=sys.stderr, flush=True)


def run(cmd: list[str], **kwargs: Any) -> subprocess.CompletedProcess[Any]:
    log(f"  $ {' '.join(cmd)}")
    return subprocess.run(cmd, check=True, **kwargs)


def run_capture(cmd: list[str]) -> str:
    result: subprocess.CompletedProcess[str] = run(cmd, capture_output=True, text=True)
    return result.stdout.strip()


def run_capture_allow_failure(cmd: list[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(cmd, capture_output=True, text=True)


def read_project_meta() -> tuple[str, str]:
    with open(ROOT / "pyproject.toml", "rb") as f:
        data = tomllib.load(f)
    project = data["project"]
    return project["name"], project["version"]


def detect_repo() -> str:
    url = run_capture(["git", "remote", "get-url", "origin"])
    if url.startswith("git@"):
        url = url.split(":", 1)[1]
    elif "github.com/" in url:
        url = url.split("github.com/", 1)[1]
    return url.removesuffix(".git")


def check_pypi_version(name: str, version: str) -> None:
    url = f"https://pypi.org/pypi/{name}/json"
    try:
        with urllib.request.urlopen(url, timeout=10) as resp:
            data = json.loads(resp.read())
    except urllib.error.HTTPError as exc:
        if exc.code == 404:
            return
        raise
    existing = set(data.get("releases", {}))
    if version in existing:
        raise SystemExit(f"{name} {version} already exists on PyPI")


def ensure_clean_and_pushed() -> None:
    dirty = run_capture(["git", "status", "--porcelain"])
    if dirty:
        raise SystemExit(f"working tree is dirty:\n{dirty}")

    local_sha = run_capture(["git", "rev-parse", "HEAD"])
    remote_sha = run_capture(["git", "rev-parse", "@{u}"])
    if local_sha != remote_sha:
        raise SystemExit(
            f"local HEAD {local_sha[:12]} differs from upstream {remote_sha[:12]}; push first"
        )


def trigger_and_wait(repo: str, workflow_file: str) -> int:
    branch = run_capture(["git", "rev-parse", "--abbrev-ref", "HEAD"])
    existing_raw = run_capture(
        [
            "gh",
            "run",
            "list",
            "--repo",
            repo,
            "--workflow",
            workflow_file,
            "--limit",
            "5",
            "--json",
            "databaseId",
        ]
    )
    existing = {row["databaseId"] for row in json.loads(existing_raw or "[]")}

    run(
        [
            "gh",
            "workflow",
            "run",
            workflow_file,
            "--repo",
            repo,
            "--ref",
            branch,
            "-f",
            "build_dist=true",
        ]
    )

    run_id: int | None = None
    for _ in range(30):
        time.sleep(2)
        result = run_capture(
            [
                "gh",
                "run",
                "list",
                "--repo",
                repo,
                "--workflow",
                workflow_file,
                "--limit",
                "10",
                "--json",
                "databaseId,status",
            ]
        )
        for row in json.loads(result or "[]"):
            if row["databaseId"] not in existing:
                run_id = row["databaseId"]
                break
        if run_id is not None:
            break
    if run_id is None:
        raise SystemExit("timed out waiting for remote build workflow to start")

    started = time.time()
    while True:
        result = run_capture(
            [
                "gh",
                "run",
                "view",
                str(run_id),
                "--repo",
                repo,
                "--json",
                "status,conclusion",
            ]
        )
        state = json.loads(result)
        if state["status"] == "completed":
            if state.get("conclusion") != "success":
                display_failure_logs(repo, run_id, workflow_file)
                raise SystemExit(
                    f"remote build failed: {state.get('conclusion')} "
                    f"https://github.com/{repo}/actions/runs/{run_id}"
                )
            log(f"  Build completed in {int(time.time() - started)}s")
            return run_id
        time.sleep(15)


def display_failure_logs(repo: str, run_id: int, workflow_file: str) -> None:
    logs_dir = DIST_DIR / "logs"
    logs_dir.mkdir(parents=True, exist_ok=True)

    artifact_download = run_capture_allow_failure(
        [
            "gh",
            "run",
            "download",
            str(run_id),
            "--repo",
            repo,
            "--pattern",
            "failure-logs-*",
            "--dir",
            str(logs_dir),
        ]
    )
    if artifact_download.returncode == 0:
        log(f"  Downloaded failure log artifacts to {logs_dir}")
        for path in sorted(logs_dir.rglob("*.log")):
            log(f"\n  --- {workflow_file} :: {path.name} ---")
            content = path.read_text(encoding="utf-8", errors="replace").splitlines()
            preview = content[-60:]
            for line in preview:
                log(f"  | {line}")
        return

    result = run_capture_allow_failure(
        ["gh", "run", "view", str(run_id), "--repo", repo, "--log-failed"]
    )
    if result.stdout:
        log(f"\n  --- {workflow_file} failed log ---")
        for line in result.stdout.splitlines()[-120:]:
            log(f"  | {line}")


def download_artifacts(repo: str, runs: dict[str, int]) -> list[Path]:
    if DIST_DIR.exists():
        shutil.rmtree(DIST_DIR)
    DIST_DIR.mkdir(parents=True)
    temp = DIST_DIR / "_tmp"
    temp.mkdir()

    for _workflow_file, run_id in runs.items():
        run(["gh", "run", "download", str(run_id), "--repo", repo, "--dir", str(temp)])

    artifact_dirs = {path.name for path in temp.iterdir() if path.is_dir()}
    missing = sorted(set(WORKFLOWS.values()) - artifact_dirs)
    if missing:
        raise SystemExit(f"missing expected artifacts: {', '.join(missing)}")

    built: list[Path] = []
    for path in temp.rglob("*"):
        if not path.is_file():
            continue
        if path.suffix not in {".whl", ".gz"}:
            continue
        target = DIST_DIR / path.name
        shutil.copy2(path, target)
        built.append(target)

    shutil.rmtree(temp)

    wheels = [path for path in built if path.suffix == ".whl"]
    sdists = [path for path in built if path.name.endswith(".tar.gz")]
    if len(wheels) < len(WORKFLOWS):
        raise SystemExit(f"expected at least {len(WORKFLOWS)} wheels, found {len(wheels)}")
    if len(sdists) != 1:
        raise SystemExit(f"expected exactly one sdist, found {len(sdists)}")
    return sorted(built)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Publish running-process from remote GitHub builds"
    )
    parser.add_argument("--dry-run", action="store_true", help="build remotely but do not upload")
    args = parser.parse_args()

    try:
        run_capture(["gh", "--version"])
    except FileNotFoundError as exc:
        raise SystemExit("gh CLI is required for remote publish flow") from exc

    name, version = read_project_meta()
    ensure_clean_and_pushed()
    if not args.dry_run:
        check_pypi_version(name, version)

    repo = detect_repo()
    log(f"Publishing {name} {version} via remote GitHub builds")
    runs = {
        workflow_file: trigger_and_wait(repo, workflow_file)
        for workflow_file in WORKFLOWS
    }
    artifacts = download_artifacts(repo, runs)

    if args.dry_run:
        log("Dry run artifacts:")
        for artifact in artifacts:
            log(f"  {artifact.name}")
        return 0

    run(["uv", "publish", *[str(path) for path in artifacts]])
    return 0


if __name__ == "__main__":
    sys.exit(main())
