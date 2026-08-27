"""Render the checked-in ConPTY hash table from release-only TOML input.

Cargo compiles build dependencies before it can evaluate a package feature.
Keeping this generator outside Cargo therefore lets an async-process-only
consumer omit TOML and every ConPTY sidecar build input entirely.  Release
automation invokes this script after producing the sidecar manifest and
before building the artifacts that need verified acquisition.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_MANIFEST = ROOT / "conpty-sidecar.sha256.toml"
DEFAULT_OUTPUT = (
    ROOT
    / "crates"
    / "running-process-platform-internal"
    / "src"
    / "platform_win"
    / "terminal"
    / "conpty_passthrough"
    / "conpty_sidecar_hashes.rs"
)
ARCHES = (
    ("x64", "EXPECTED_X64"),
    ("arm64", "EXPECTED_ARM64"),
    ("x86", "EXPECTED_X86"),
    ("arm", "EXPECTED_ARM"),
)


def assets_from_manifest(manifest: Path) -> dict[str, dict[str, str]]:
    """Read the deliberately narrow, release-owned sidecar manifest format.

    This avoids a Python-version-specific TOML dependency: the release input
    has only `[asset.<arch>]`, `sha256`, and `size_bytes` entries.
    """
    assets: dict[str, dict[str, str]] = {}
    current: dict[str, str] | None = None
    for raw_line in manifest.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        match = re.fullmatch(r"\[asset\.(x64|arm64|x86|arm)]", line)
        if match:
            current = assets.setdefault(match.group(1), {})
            continue
        if not line or line.startswith("#") or current is None:
            continue
        key, separator, value = line.partition("=")
        if separator and key.strip() in {"sha256", "size_bytes"}:
            current[key.strip()] = value.strip().strip('"')
    return assets


def render(manifest: Path) -> str:
    assets = assets_from_manifest(manifest)
    lines = [
        "//! Compile-time SHA-256 verification table for the Win10 ConPTY sidecar (#447).",
        "//!",
        "//! This generated source is checked in so normal platform builds have no",
        "//! build script, TOML parser, or manifest read. The release workflow derives",
        "//! this file from `conpty-sidecar.sha256.toml` before it builds artifacts; the",
        "//! TOML file is release input, not a compile-time dependency.",
        "//!",
        "//! Pre-release dev checkouts ship with an empty manifest, so every const",
        '//! below is `None` and the runtime falls back to "fetch only, no verify."',
        "",
        "#![cfg(windows)]",
        "",
        "pub(super) struct ExpectedAsset {",
        "    pub sha256_hex: &'static str,",
        "    pub size_bytes: u64,",
        "}",
        "",
        "// This repository checkout intentionally has no released sidecar hashes.",
        "// `ci/render_conpty_sidecar_hashes.py` replaces these constants in the",
        "// release checkout before wheel, binary, and crate builds.",
        "",
    ]
    for arch, constant in ARCHES:
        asset = assets.get(arch)
        if asset is None:
            value = "None"
        else:
            try:
                digest = asset["sha256"]
                size = int(asset["size_bytes"])
            except KeyError as error:
                raise ValueError(f"asset.{arch} is missing {error.args[0]}") from error
            if len(digest) != 64 or any(
                char not in "0123456789abcdef" for char in digest.lower()
            ):
                raise ValueError(
                    f"asset.{arch}.sha256 must be a 64-character hexadecimal SHA-256"
                )
            if size < 0:
                raise ValueError(f"asset.{arch}.size_bytes must be non-negative")
            value = (
                f'Some(ExpectedAsset {{ sha256_hex: "{digest}", size_bytes: {size} }})'
            )
        lines.extend(
            (
                "#[allow(dead_code)]",
                f"pub(super) const {constant}: Option<ExpectedAsset> = {value};",
            )
        )
    lines.extend(
        (
            "",
            "/// Returns the verification baseline for the current build's target arch,",
            "/// if the manifest carried one. `None` means the runtime should fetch",
            "/// without verifying (and log a diagnostic line on opt-in).",
            "pub(super) fn expected_for_current_arch() -> Option<&'static ExpectedAsset> {",
            '    #[cfg(target_arch = "x86_64")]',
            "    {",
            "        EXPECTED_X64.as_ref()",
            "    }",
            '    #[cfg(target_arch = "aarch64")]',
            "    {",
            "        EXPECTED_ARM64.as_ref()",
            "    }",
            '    #[cfg(target_arch = "x86")]',
            "    {",
            "        EXPECTED_X86.as_ref()",
            "    }",
            '    #[cfg(target_arch = "arm")]',
            "    {",
            "        EXPECTED_ARM.as_ref()",
            "    }",
            "    #[cfg(not(any(",
            '        target_arch = "x86_64",',
            '        target_arch = "aarch64",',
            '        target_arch = "x86",',
            '        target_arch = "arm"',
            "    )))]",
            "    {",
            "        None",
            "    }",
            "}",
            "",
        )
    )
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail when the checked-in table differs from rendered release input",
    )
    args = parser.parse_args(argv)
    expected = render(args.manifest)
    if args.check:
        actual = args.output.read_text(encoding="utf-8")
        if actual != expected:
            print(
                f"ConPTY hash table is stale: run the renderer for {args.manifest}",
                file=sys.stderr,
            )
            return 1
        return 0
    args.output.write_text(expected, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
