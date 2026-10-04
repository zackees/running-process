"""Replay the real Linux quick gate through Bosn and verify its source proof.

This helper is run by the attesting wrapper, once local-gate.toml is wired.
It never runs product tests on the developer host and never stamps a commit.
Only a clean native Linux x64 run with every required step completed can pass.
"""

from __future__ import annotations

import argparse
import json
import platform
import re
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import TypeAlias

JsonValue: TypeAlias = str | int | float | bool | list["JsonValue"] | dict[str, "JsonValue"] | None
ROOT = Path(__file__).resolve().parents[1]
CI_LINT_REF = "86b63937960d00655f7ef3752ef6f15b6b06f35b"
WORKFLOW = ".github/workflows/ci.yml"
SELECTED_JOB = "linux-quick"


@dataclass(frozen=True)
class RequiredJob:
    job_id: str
    steps: tuple[str, ...]


REQUIRED_JOBS = (
    RequiredJob("ci-mode", ("Local gate attestation",)),
    RequiredJob(
        "preflight",
        (
            "cargo build (workspace + all-targets, debug, --features client)",
            "Dev wheel build",
            "Lint",
            "Record kernel substrate timing",
            "Async semantic capture contract (kernel substrate)",
            "Direct backend identity contract",
            "Frame v1 codec contract",
            "Daemon registration contract",
            "Daemon registration v2 writer contract",
            "Terminal graphics capability matrix",
            "Report terminal graphics capability matrix",
            "Unit Tests",
            "Broker Hello perf guard",
        ),
    ),
    RequiredJob(
        "dylint",
        (
            "Dylint negative fixture",
            "Dylint platform-boundary fixture",
            "Dylint workspace gate",
        ),
    ),
    RequiredJob("lint-gates", ("Require preflight and requested Dylint to have passed",)),
)


@dataclass(frozen=True)
class Section:
    name: str
    stage: str
    status: str
    conclusion: str


@dataclass(frozen=True)
class Job:
    job_id: str
    status: str
    conclusion: str
    sections: tuple[Section, ...]


@dataclass(frozen=True)
class Proof:
    workspace: Path
    sha: str
    act_version: str
    jobs: tuple[Job, ...]


@dataclass(frozen=True)
class Captured:
    returncode: int
    output: str


def document(value: JsonValue) -> dict[str, JsonValue]:
    """Validate a JSON object only at the receipt boundary."""
    if not isinstance(value, dict):
        raise ValueError("expected a JSON object")
    return value


def values(value: JsonValue) -> list[JsonValue]:
    if not isinstance(value, list):
        raise ValueError("expected a JSON array")
    return value


def text(value: JsonValue) -> str:
    if not isinstance(value, str):
        raise ValueError("expected a JSON string")
    return value


def integer(value: JsonValue) -> int:
    if type(value) is not int:
        raise ValueError("expected a JSON integer")
    return value


def _section(value: JsonValue) -> Section:
    raw = document(value)
    return Section(*(text(raw[key]) for key in ("name", "stage", "status", "conclusion")))


def _job(value: JsonValue) -> Job:
    raw = document(value)
    # Non-required setup/upload steps may be skipped; they are not proof.
    sections = tuple(
        _section(section)
        for section in values(raw["sections"])
        if document(section).get("stage") == "Main"
    )
    return Job(text(raw["job_id"]), text(raw["status"]), text(raw["conclusion"]), sections)


def _parse_proof(output: str) -> Proof:
    documents: list[dict[str, JsonValue]] = [
        document(json.loads(line)) for line in output.splitlines() if line.startswith("{")
    ]
    if len(documents) != 1:
        raise ValueError("expected one unambiguous terminal Bosn receipt")
    raw = documents[0]
    if raw["dirty"] is not None:
        raise ValueError("Bosn executed a dirty snapshot")
    if integer(raw["schema_version"]) != 1 or integer(raw["exit_code"]) != 0:
        raise ValueError("unknown receipt schema or nonzero exit code")
    expected = {
        "state": "done",
        "conclusion": "success",
        "engine": "act",
        "event": "pull_request",
        "workflow": WORKFLOW,
        "job": SELECTED_JOB,
        "mode": "minimal",
    }
    if any(raw[key] != value for key, value in expected.items()):
        raise ValueError("Bosn did not successfully execute the declared quick selection")
    tree = document(raw["tree"])
    if integer(tree["malformed_lines"]) != 0:
        raise ValueError("Bosn could not parse every workflow log record")
    jobs = tuple(
        _job(job) for group in values(tree["groups"]) for job in values(document(group)["jobs"])
    )
    return Proof(Path(text(raw["workspace"])), text(raw["sha"]), text(raw["act_version"]), jobs)


def _jobs_error(jobs: tuple[Job, ...]) -> str | None:
    expected_ids = {job.job_id for job in REQUIRED_JOBS}
    if len(jobs) != len(expected_ids) or {job.job_id for job in jobs} != expected_ids:
        return "missing, duplicate or unexpected Bosn jobs"
    for required in REQUIRED_JOBS:
        job = next(job for job in jobs if job.job_id == required.job_id)
        if job.status != "completed" or job.conclusion != "success":
            return f"Bosn job {job.job_id} did not pass"
        completed = {
            section.name
            for section in job.sections
            if section.status == "completed" and section.conclusion == "success"
        }
        if not set(required.steps).issubset(completed):
            return f"Bosn job {job.job_id} did not execute every required step"
    return None


def proof_error(output: str, *, workspace: Path, head_sha: str) -> str | None:
    try:
        proof = _parse_proof(output)
        if not proof.workspace.is_absolute() or proof.workspace.resolve() != workspace.resolve():
            return "Bosn executed another workspace"
        if proof.sha != head_sha:
            return "Bosn executed another commit"
        match = re.fullmatch(r"\d+\.\d+\.\d+-act2\.(\d+)", proof.act_version)
        if not match or int(match[1]) < 3:
            return "Bosn did not use the released act2 runner"
        return _jobs_error(proof.jobs)
    except (KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        return f"invalid Bosn proof: {error}"


def fidelity_error(host_arch: str, daemon: str, returncode: int) -> str | None:
    if host_arch.lower() not in {"x86_64", "amd64"}:
        return "Linux tests require a native x64 host CPU"
    if returncode != 0 or daemon.strip().lower() not in {"linux x86_64", "linux amd64"}:
        return "Linux tests require a Linux x64 Docker daemon"
    return None


def run_captured(argv: list[str]) -> Captured:
    """Use files so a daemon inheriting output cannot hold a pipe open."""
    with tempfile.TemporaryFile() as output:
        child = subprocess.run(argv, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT, check=False)
        output.seek(0)
        return Captured(child.returncode, output.read().decode("utf-8", errors="replace"))


def _head() -> str:
    status = run_captured(["git", "status", "--porcelain"])
    head = run_captured(["git", "rev-parse", "HEAD"])
    if status.returncode or status.output.strip() or head.returncode:
        raise ValueError("local gate requires a clean committed worktree")
    return head.output.strip()


def command() -> list[str]:
    return [
        "bosn",
        "ci",
        "run",
        "--workspace",
        str(ROOT),
        "--workflow",
        WORKFLOW,
        "--job",
        SELECTED_JOB,
        "--trigger",
        "pr",
        "--mode",
        "minimal",
        "--timeout-secs",
        "3600",
        "--wait",
        "--deadline-ms",
        "3600000",
        "--json",
    ]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lane", choices=("linux-quick",), default="linux-quick")
    parser.add_argument("--list", action="store_true")
    args = parser.parse_args(argv)
    if args.list:
        print(" ".join(command()))
        return 0
    try:
        head = _head()
        daemon = run_captured(["docker", "info", "--format", "{{.OSType}} {{.Architecture}}"])
        error = fidelity_error(platform.machine(), daemon.output, daemon.returncode)
        if error:
            raise ValueError(error)
        print("local gate: replaying Linux quick through Bosn", flush=True)
        result = run_captured(command())
        logs = ROOT / "target" / "local-gate-logs"
        logs.mkdir(parents=True, exist_ok=True)
        (logs / "linux-quick.log").write_text(result.output, encoding="utf-8")
        if _head() != head:
            raise ValueError("source changed while Bosn executed")
        error = proof_error(result.output, workspace=ROOT, head_sha=head)
        if result.returncode or error:
            print(result.output[-12000:])
            raise ValueError(error or f"Bosn exited {result.returncode}")
        print("local gate: Linux quick passed with clean source and executed-step proof")
        return 0
    except (OSError, ValueError) as error:
        print(f"local gate: {error}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
