"""Writable fixture staging for immutable-source consumer checks."""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import tempfile
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path


def consumer_target_dir(root: Path, family: str) -> Path:
    configured = os.environ.get("CARGO_TARGET_DIR")
    target_root = (
        Path(configured)
        if configured
        else Path(tempfile.gettempdir()) / "running-process-contract-target"
    )
    if not target_root.is_absolute():
        target_root = root / target_root
    return target_root / family


def stage_consumer(consumer_root: Path, name: str, scratch: Path) -> Path:
    source = consumer_root / name
    staged = scratch / consumer_root.name / name
    shutil.copytree(source, staged)
    manifest = staged / "Cargo.toml"
    text = manifest.read_text(encoding="utf-8")

    def absolute_dependency(match: re.Match[str]) -> str:
        dependency = (source / match.group(1)).resolve()
        return "path = " + json.dumps(str(dependency))

    text = re.sub(r'path\s*=\s*"([^"\n]+)"', absolute_dependency, text)
    manifest.write_text(text, encoding="utf-8")
    return manifest


@dataclass(frozen=True)
class CommandResult:
    returncode: int
    output: str


def run_command(
    root: Path, command: tuple[str, ...], environment: Mapping[str, str] | None = None
) -> CommandResult:
    # File-backed output cannot fill a pipe while Cargo runs.
    with tempfile.TemporaryFile(mode="w+b") as output:
        result = subprocess.run(
            command, cwd=root, stdout=output, stderr=subprocess.STDOUT, check=False, env=environment
        )
        output.seek(0)
        return CommandResult(result.returncode, output.read().decode("utf-8", errors="replace"))
