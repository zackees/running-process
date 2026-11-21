"""Replay the real Linux quick gate through Bosn and verify its source proof.

This helper is run by the attesting wrapper, once local-gate.toml is wired.
It never runs product tests on the developer host and never stamps a commit.
Only a clean native Linux x64 run with every required step completed can pass.
The existing quick dispatch runs before PR attestation verification can enforce
the proof this local execution is about to produce.

Lint never runs under bosn; soldr lint only. The replay dispatches with
``host_lint=true``, which skips the preflight ``Lint`` step and the ``dylint``
job, and the proof rejects a receipt in which either executed. ``--lane lint``
runs that lint on the host: ``soldr lint rust --check`` (rustfmt, Clippy and the
configured Dylint libraries, plus the platform-boundary Dylint as a
``[workspace.metadata.soldr.lint]`` extra), the two Dylint fixture suites, and
``ci.lint --no-rust`` (the repository's Python guards, contracts and Ruff).
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path, PurePath
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
    RequiredJob("lint-gates", ("Require preflight and requested Dylint to have passed",)),
)
# Lint never runs under bosn: with host_lint=true these must not execute. The
# `dylint` job may be reported skipped or omitted; preflight's `Lint` step may
# be reported skipped or omitted. Either one executing rejects the receipt.
SKIPPED_JOBS = frozenset({"dylint"})
SKIPPED_STEPS = {"preflight": frozenset({"Lint"})}
NIGHTLY = "nightly-2026-08-15"
LINT_FIXTURES = ("lints/running-process-env-literal", "lints/running-process-platform-boundary")


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
        "event": "workflow_dispatch",
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
    for skipped in SKIPPED_JOBS:
        if sum(job.job_id == skipped for job in jobs) > 1:
            return "missing, duplicate or unexpected Bosn jobs"
    for job in jobs:
        if job.job_id in SKIPPED_JOBS:
            if job.conclusion != "skipped" or any(
                section.conclusion != "skipped" for section in job.sections
            ):
                return f"Bosn executed lint job {job.job_id}; lint runs on the host"
    jobs = tuple(job for job in jobs if job.job_id not in SKIPPED_JOBS)
    if len(jobs) != len(expected_ids) or {job.job_id for job in jobs} != expected_ids:
        return "missing, duplicate or unexpected Bosn jobs"
    for job in jobs:
        lint_steps = SKIPPED_STEPS.get(job.job_id, frozenset())
        if any(s.name in lint_steps and s.conclusion != "skipped" for s in job.sections):
            return f"Bosn executed a lint step in {job.job_id}; lint runs on the host"
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


def _is_rooted(path: PurePath) -> bool:
    """Whether a receipt's workspace names a location rather than a relative path.

    Bosn executes the gate inside a Linux container, so its receipt reports a
    POSIX workspace such as ``/work/repo``. ``Path.is_absolute()`` is false for
    that on Windows, where an absolute path also needs a drive, so the check
    rejected every genuine receipt there. A root is what separates a located
    path from ``relative/repo`` (or a drive-relative ``C:repo``) on every host;
    the ``resolve()`` comparison that follows still pins the exact workspace.
    """
    return bool(path.root)


def proof_error(output: str, *, workspace: Path, head_sha: str) -> str | None:
    try:
        proof = _parse_proof(output)
        if not _is_rooted(proof.workspace) or proof.workspace.resolve() != workspace.resolve():
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


def _static_checks() -> None:
    for module in (
        "ci.platform_boundary",
        "ci.async_compliance_guard",
        "ci.cache_save_guard",
    ):
        result = run_captured([sys.executable, "-m", module])
        if result.returncode:
            raise ValueError(f"{module} failed before engine submission:\n{result.output}")


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
        "--event",
        "workflow_dispatch",
        "--input",
        "full=false",
        "--input",
        "host_lint=true",
        "--mode",
        "minimal",
        "--timeout-secs",
        "3600",
        "--wait",
        "--deadline-ms",
        "3600000",
        "--json",
    ]


def lint_commands() -> list[tuple[list[str], Path]]:
    """The host lint lane: everything the replay skips, never under bosn."""
    fixtures = [
        (["soldr", "cargo", f"+{NIGHTLY}", "test", "--locked"], ROOT / crate)
        for crate in LINT_FIXTURES
    ]
    return [
        (["soldr", "lint", "rust", "--check"], ROOT),
        *fixtures,
        (["uv", "sync", "--no-editable"], ROOT),
        (["uv", "run", "--no-sync", "--no-editable", "-m", "ci.lint", "--no-rust"], ROOT),
    ]


def lint_env() -> dict[str, str]:
    """The CI `dylint` job's environment (and ci/dylint_gate.py's)."""
    return {
        **os.environ,
        "SOLDR_DYLINT_TOOLCHAIN": NIGHTLY,
        "SOLDR_DYLINT_DRIVER_FALLBACK": "off",
        # The lint crates select dylint-link through cfg(all()) (soldr#3483).
        "SOLDR_LINKER": "default",
        "RUNNING_PROCESS_REQUIRE_DYLINT": "1",
    }


def run_lint_lane() -> int:
    try:
        head = _head()
    except ValueError as error:
        print(f"local gate: {error}")
        return 1
    env = lint_env()
    for argv, cwd in lint_commands():
        print(f"local gate lint: {' '.join(argv)}", flush=True)
        lane_env = env
        if cwd != ROOT:
            # Linked fixtures get their own target dir, as in CI.
            lane_env = {**env, "CARGO_BIN": "cargo",
                        "CARGO_TARGET_DIR": str(ROOT / "target" / "dylint-fixtures")}
        if subprocess.run(argv, cwd=cwd, env=lane_env, check=False).returncode:
            print(f"local gate: lint failed: {' '.join(argv)}")
            return 1
    try:
        if _head() != head:
            raise ValueError("source changed while the lint lane ran")
    except ValueError as error:
        print(f"local gate: {error}")
        return 1
    print("local gate: host lint (soldr lint) passed")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lane", choices=("linux-quick", "lint"), default="linux-quick")
    parser.add_argument("--list", action="store_true")
    args = parser.parse_args(argv)
    if args.lane == "lint":
        if args.list:
            print("\n".join(" ".join(argv) for argv, _ in lint_commands()))
            return 0
        return run_lint_lane()
    if args.list:
        print(" ".join(command()))
        return 0
    try:
        head = _head()
        _static_checks()
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
