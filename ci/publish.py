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
WORKFLOW_FILE = "build.yml"
EXPECTED_ARTIFACTS = {
    "wheels-linux-x86",
    "wheels-linux-arm",
    "wheels-windows-x86",
    "wheels-windows-arm",
    "wheels-macos-x86",
    "wheels-macos-arm",
}


def log(msg: str) -> None:
    print(msg, file=sys.stderr, flush=True)


def run(cmd: list[str], **kwargs: Any) -> subprocess.CompletedProcess[Any]:
    log(f"  $ {' '.join(cmd)}")
    return subprocess.run(cmd, check=True, **kwargs)


def run_capture(cmd: list[str]) -> str:
    result: subprocess.CompletedProcess[str] = run(cmd, capture_output=True, text=True)
    return result.stdout.strip()


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


def trigger_and_wait(repo: str) -> int:
    branch = run_capture(["git", "rev-parse", "--abbrev-ref", "HEAD"])
    existing_raw = run_capture(
        [
            "gh",
            "run",
            "list",
            "--repo",
            repo,
            "--workflow",
            WORKFLOW_FILE,
            "--limit",
            "5",
            "--json",
            "databaseId",
        ]
    )
    existing = {row["databaseId"] for row in json.loads(existing_raw or "[]")}

    run(["gh", "workflow", "run", WORKFLOW_FILE, "--repo", repo, "--ref", branch])

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
                WORKFLOW_FILE,
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
                raise SystemExit(
                    f"remote build failed: {state.get('conclusion')} "
                    f"https://github.com/{repo}/actions/runs/{run_id}"
                )
            log(f"  Build completed in {int(time.time() - started)}s")
            return run_id
        time.sleep(15)


def download_artifacts(repo: str, run_id: int) -> list[Path]:
    if DIST_DIR.exists():
        shutil.rmtree(DIST_DIR)
    DIST_DIR.mkdir(parents=True)
    temp = DIST_DIR / "_tmp"
    temp.mkdir()

    run(["gh", "run", "download", str(run_id), "--repo", repo, "--dir", str(temp)])

    artifact_dirs = {path.name for path in temp.iterdir() if path.is_dir()}
    missing = sorted(EXPECTED_ARTIFACTS - artifact_dirs)
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
    if len(wheels) < len(EXPECTED_ARTIFACTS):
        raise SystemExit(f"expected at least {len(EXPECTED_ARTIFACTS)} wheels, found {len(wheels)}")
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
    log(f"Publishing {name} {version} via remote workflow {WORKFLOW_FILE}")
    run_id = trigger_and_wait(repo)
    artifacts = download_artifacts(repo, run_id)

    if args.dry_run:
        log("Dry run artifacts:")
        for artifact in artifacts:
            log(f"  {artifact.name}")
        return 0

    run(["uv", "publish", *[str(path) for path in artifacts]])
    return 0


if __name__ == "__main__":
    sys.exit(main())
