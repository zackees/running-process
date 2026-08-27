"""Structural dependency-ownership guard for #1146."""

from __future__ import annotations

import sys
import unittest
from collections.abc import Mapping
from pathlib import Path
from tempfile import TemporaryDirectory

import tomllib

ROOT = Path(__file__).resolve().parent.parent
INTERNAL_MANIFEST = ROOT / "crates" / "running-process-platform-internal" / "Cargo.toml"
ROOT_MANIFEST = ROOT / "crates" / "running-process" / "Cargo.toml"
BUILD_SCRIPT = INTERNAL_MANIFEST.parent / "build.rs"
HASH_TABLE = (
    INTERNAL_MANIFEST.parent
    / "src"
    / "platform_win"
    / "terminal"
    / "conpty_passthrough"
    / "conpty_sidecar_hashes.rs"
)


def load_manifest(path: Path) -> dict[str, object]:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def feature_members(manifest: Mapping[str, object], feature: str) -> set[str] | None:
    features = manifest.get("features")
    if not isinstance(features, Mapping):
        return None
    members = features.get(feature)
    if not isinstance(members, list) or not all(
        isinstance(member, str) for member in members
    ):
        return None
    return set(members)


def dependency(
    manifest: Mapping[str, object], name: str
) -> Mapping[str, object] | None:
    dependencies = manifest.get("dependencies")
    if not isinstance(dependencies, Mapping):
        return None
    candidate = dependencies.get(name)
    return candidate if isinstance(candidate, Mapping) else None


def version_at_least(requirement: object, minimum: tuple[int, int, int]) -> bool:
    if not isinstance(requirement, str):
        return False
    numeric = requirement.lstrip("^~=>< ").split("-", 1)[0]
    parts = numeric.split(".")
    if not all(part.isdigit() for part in parts) or not 1 <= len(parts) <= 3:
        return False
    parsed = tuple(int(part) for part in parts) + (0,) * (3 - len(parts))
    return parsed >= minimum


def require_feature(
    manifest: Mapping[str, object],
    feature: str,
    members: set[str],
    failures: list[str],
    label: str,
) -> None:
    selected = feature_members(manifest, feature)
    if selected is None:
        failures.append(f"{label}: missing feature `{feature}`")
        return
    missing = sorted(members - selected)
    if missing:
        failures.append(
            f"{label}: feature `{feature}` must compose {', '.join(missing)}"
        )


def check_manifests(
    internal: Mapping[str, object],
    root: Mapping[str, object],
    *,
    build_script_exists: bool,
    table: str,
) -> list[str]:
    failures: list[str] = []
    if build_script_exists:
        failures.append("platform-internal must not restore a build.rs")
    build_dependencies = internal.get("build-dependencies")
    if isinstance(build_dependencies, Mapping) and build_dependencies:
        failures.append("platform-internal must not restore build dependencies")

    internal_default = feature_members(internal, "default")
    if internal_default != {"async-process"}:
        failures.append(
            "platform-internal default must preserve the former async process surface"
        )
    require_feature(
        internal,
        "async-process",
        {"dep:tokio", "process-inspection"},
        failures,
        "platform-internal",
    )
    require_feature(
        internal,
        "process-inspection",
        {"dep:sysinfo"},
        failures,
        "platform-internal",
    )
    require_feature(
        internal,
        "pty",
        {"dep:portable-pty", "process-inspection"},
        failures,
        "platform-internal",
    )
    require_feature(
        internal,
        "ipc",
        {"dep:interprocess", "dep:dirs", "dep:blake3"},
        failures,
        "platform-internal",
    )
    require_feature(
        internal,
        "ipc-async",
        {"ipc", "async-process", "interprocess/tokio", "tokio/net"},
        failures,
        "platform-internal",
    )

    tokio = dependency(internal, "tokio")
    if tokio is None or tokio.get("optional") is not True:
        failures.append("platform-internal Tokio must remain optional")
    sysinfo = dependency(internal, "sysinfo")
    if sysinfo is None or sysinfo.get("optional") is not True:
        failures.append("platform-internal sysinfo must remain optional")
    blake3 = dependency(internal, "blake3")
    if (
        blake3 is None
        or blake3.get("optional") is not True
        or not version_at_least(blake3.get("version"), (1, 8, 7))
    ):
        failures.append("platform-internal blake3 must be optional and require >=1.8.7")

    platform_dependency = dependency(root, "running-process-platform-internal")
    if (
        platform_dependency is None
        or platform_dependency.get("default-features") is not False
        or platform_dependency.get("features") != ["process-inspection"]
    ):
        failures.append(
            "root must opt out of platform defaults and select only Phase 0.5 containment"
        )
    require_feature(
        root,
        "async-process",
        {
            "dep:tokio",
            "process-inspection",
            "running-process-platform-internal/async-process",
        },
        failures,
        "root",
    )
    require_feature(
        root,
        "pty",
        {"process-inspection", "running-process-platform-internal/pty"},
        failures,
        "root",
    )
    require_feature(
        root,
        "process-inspection",
        {"dep:sysinfo", "running-process-platform-internal/process-inspection"},
        failures,
        "root",
    )

    if 'include!(concat!(env!("OUT_DIR")' in table:
        failures.append(
            "ConPTY hash table must be checked-in source, not OUT_DIR output"
        )
    for constant in ("EXPECTED_X64", "EXPECTED_ARM64", "EXPECTED_X86", "EXPECTED_ARM"):
        if constant not in table:
            failures.append(f"ConPTY hash table is missing {constant}")
    return failures


def check() -> list[str]:
    return check_manifests(
        load_manifest(INTERNAL_MANIFEST),
        load_manifest(ROOT_MANIFEST),
        build_script_exists=BUILD_SCRIPT.exists(),
        table=HASH_TABLE.read_text(encoding="utf-8"),
    )


class MinimalAsyncPlatformGraphTests(unittest.TestCase):
    def test_real_manifests_satisfy_the_contract(self) -> None:
        assert check() == []

    def test_comments_dev_dependencies_and_unrelated_text_do_not_satisfy_contract(
        self,
    ) -> None:
        internal = tomllib.loads(
            """
            # [build-dependencies]
            [features]
            default = ["async-process"]
            async-process = ["dep:tokio", "process-inspection"]
            process-inspection = ["dep:sysinfo"]
            pty = ["dep:portable-pty", "process-inspection"]
            ipc = ["dep:interprocess", "dep:dirs", "dep:blake3"]
            ipc-async = ["ipc", "async-process", "interprocess/tokio", "tokio/net"]
            [dependencies]
            tokio = { version = "1", optional = false }
            sysinfo = { version = "0.30", optional = true }
            blake3 = { version = "1.8.7", optional = true }
            unrelated = "optional = true; dep:tokio"
            [dev-dependencies]
            tokio = { version = "1", optional = true }
            """
        )
        root = tomllib.loads(
            """
            [features]
            async-process = [
                "dep:tokio",
                "process-inspection",
                "running-process-platform-internal/async-process",
            ]
            pty = [
                "process-inspection",
                "running-process-platform-internal/pty",
            ]
            process-inspection = [
                "dep:sysinfo",
                "running-process-platform-internal/process-inspection",
            ]
            [dependencies]
            [dependencies.running-process-platform-internal]
            version = "4.10.6"
            default-features = false
            features = ["process-inspection"]
            """
        )
        failures = check_manifests(
            internal,
            root,
            build_script_exists=False,
            table="EXPECTED_X64 EXPECTED_ARM64 EXPECTED_X86 EXPECTED_ARM",
        )
        assert "platform-internal Tokio must remain optional" in failures
        assert not any("build dependencies" in failure for failure in failures)

    def test_actual_build_dependencies_fail_even_when_a_comment_claims_none(
        self,
    ) -> None:
        internal = load_manifest(INTERNAL_MANIFEST)
        root = load_manifest(ROOT_MANIFEST)
        mutated = dict(internal)
        mutated["build-dependencies"] = {"toml": "0.8"}
        failures = check_manifests(
            mutated,
            root,
            build_script_exists=False,
            table=HASH_TABLE.read_text(encoding="utf-8"),
        )
        assert "platform-internal must not restore build dependencies" in failures

    def test_loader_reads_toml_not_a_string_heuristic(self) -> None:
        with TemporaryDirectory() as temporary:
            path = Path(temporary) / "Cargo.toml"
            path.write_text(
                '# tokio = { version = "1", optional = true }\n[dependencies]\nname = "x"\n',
                encoding="utf-8",
            )
            assert load_manifest(path)["dependencies"] == {"name": "x"}


def main() -> int:
    failures = check()
    if failures:
        print("minimal async platform graph guard failed:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    print("minimal async platform graph guard passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
