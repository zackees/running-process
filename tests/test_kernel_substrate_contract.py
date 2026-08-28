"""Collected semantic fixtures for the #1147 resolver contract."""

from __future__ import annotations

import unittest

import tomllib

from ci.kernel_substrate_contract import (
    FEATURE,
    SUPPORTED_WINDOWS_TARGETS,
    graph_failures,
    load_allowlist,
    load_manifest,
    manifest_failures,
    package_names,
    tree_command,
)


class KernelSubstrateContractTests(unittest.TestCase):
    def test_real_manifest_declares_the_semantic_feature(self) -> None:
        self.assertEqual(manifest_failures(load_manifest()), [])

    def test_comments_dev_dependencies_and_alias_text_do_not_satisfy_the_feature(self) -> None:
        manifest = tomllib.loads(
            """
            # kernel-substrate = [\"async-process\"]
            [features]
            async-process = []
            [dev-dependencies]
            kernel-substrate = { package = \"async-process\", version = \"1\" }
            """
        )
        self.assertIn(
            'kernel-substrate must be exactly ["async-process"]', manifest_failures(manifest)
        )

    def test_resolved_package_parser_uses_package_names_not_aliases_or_comments(self) -> None:
        packages = package_names("# rusqlite v0.32\nlocal-db v0.1\nrusqlite v0.32\n")
        self.assertEqual(packages, {"local-db", "rusqlite"})

    def test_forbidden_and_unknown_resolved_packages_fail(self) -> None:
        allowlist = {"packages": {"running-process": "root"}, "forbidden": ["rusqlite"]}
        failures = graph_failures("running-process v1\nrusqlite v1\nunknown v1\n", allowlist)
        self.assertIn("forbidden package resolved: rusqlite", failures)
        self.assertIn("unreviewed resolved package: unknown", failures)

    def test_forbidden_feature_alias_cannot_join_the_selection(self) -> None:
        manifest = {"features": {FEATURE: ["async-process", "pty"]}}
        self.assertTrue(manifest_failures(manifest))

    def test_supported_windows_target_graphs_are_explicit_and_targeted(self) -> None:
        self.assertEqual(
            SUPPORTED_WINDOWS_TARGETS,
            ("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"),
        )
        command = tree_command("aarch64-pc-windows-msvc")
        self.assertEqual(command[-2:], ("--target", "aarch64-pc-windows-msvc"))

    def test_windows_target_metadata_requires_a_reviewed_rationale(self) -> None:
        graph = "running-process v1\nwindows_aarch64_msvc v1\n"
        failures = graph_failures(graph, {"packages": {"running-process": "root"}})
        self.assertIn("unreviewed resolved package: windows_aarch64_msvc", failures)
        failures = graph_failures(
            graph,
            {
                "packages": {
                    "running-process": "root",
                    "windows_aarch64_msvc": "Windows ARM64 import metadata.",
                }
            },
        )
        self.assertEqual(failures, [])

    def test_macos_sysinfo_process_enumeration_abi_is_reviewed(self) -> None:
        packages = load_allowlist().get("packages", {})
        self.assertIsInstance(packages, dict)
        self.assertIn("core-foundation-sys", packages)
        self.assertIn("sysinfo", str(packages["core-foundation-sys"]))
