"""Feature-graph contract for #1145 product-only root dependencies.

The assertions are structural so they catch a dependency-shape regression
before a resolver or compiler run: a process-only consumer must not select
serialization, host inspection, terminal graphics, or the trampoline binary.
"""

from __future__ import annotations

import unittest
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "crates" / "running-process" / "Cargo.toml"
LIB = ROOT / "crates" / "running-process" / "src" / "lib.rs"
TRAMPOLINE = ROOT / "crates" / "running-process" / "src" / "bin" / "trampoline.rs"
TERMINAL_GRAPHICS = ROOT / "crates" / "running-process" / "src" / "terminal_graphics.rs"
ORIGINATOR = ROOT / "crates" / "running-process" / "src" / "originator.rs"
PROBE_WORKER = ROOT / "crates" / "running-process" / "src" / "probe" / "worker.rs"
RUNPM_CONFIG = ROOT / "crates" / "running-process" / "src" / "runpm_config.rs"
SPAWN = ROOT / "crates" / "running-process" / "src" / "spawn.rs"


class TestProductDependencyFeatures(unittest.TestCase):
    def setUp(self) -> None:
        self.manifest = tomllib.loads(MANIFEST.read_text(encoding="utf-8"))
        self.features = self.manifest["features"]
        self.dependencies = self.manifest["dependencies"]

    def test_process_only_dependencies_are_optional(self) -> None:
        """RED before #1145: serde, JSON, and sysinfo were baseline deps."""
        for name in ("serde", "serde_json", "sysinfo"):
            self.assertTrue(self.dependencies[name]["optional"], name)

    def test_capabilities_own_product_dependencies(self) -> None:
        self.assertIn("dep:serde", self.features["client"])
        self.assertIn("dep:serde_json", self.features["client"])
        self.assertIn("terminal-graphics", self.features["client"])
        self.assertIn("terminal-graphics", self.features["pty"])
        self.assertEqual(self.features["terminal-graphics"], ["dep:serde"])
        self.assertEqual(
            self.features["daemon-trampoline"], ["dep:serde", "dep:serde_json"]
        )
        self.assertIn("process-inspection", self.features["daemon"])
        self.assertIn("process-inspection", self.features["originator-scan"])
        self.assertIn("process-inspection", self.features["probe"])
        self.assertEqual(
            self.features["process-inspection"],
            ["dep:sysinfo", "running-process-platform-internal/process-inspection"],
        )

    def test_binaries_and_source_are_gated_by_their_owners(self) -> None:
        trampoline = next(
            bin_ for bin_ in self.manifest["bin"] if bin_["name"] == "daemon-trampoline"
        )
        self.assertEqual(trampoline["required-features"], ["daemon-trampoline"])
        self.assertIn('#[cfg(feature = "terminal-graphics")]', LIB.read_text(encoding="utf-8"))
        self.assertIn("serde::Deserialize", TRAMPOLINE.read_text(encoding="utf-8"))
        self.assertIn(
            "use serde::{Deserialize, Serialize};",
            TERMINAL_GRAPHICS.read_text(encoding="utf-8"),
        )
        self.assertIn("sysinfo::", ORIGINATOR.read_text(encoding="utf-8"))
        self.assertIn("sysinfo::", PROBE_WORKER.read_text(encoding="utf-8"))
        self.assertIn("serde::Deserialize", RUNPM_CONFIG.read_text(encoding="utf-8"))

    def test_client_only_spawn_wire_tests_are_gated(self) -> None:
        spawn = SPAWN.read_text(encoding="utf-8")
        self.assertIn(
            '#[cfg(feature = "client")]\n    use prost::Message;',
            spawn,
        )
        for fixture in ("LegacyClearAtTag4", "LegacyClearAtTag5"):
            self.assertIn(
                f'#[cfg(feature = "client")]\n'
                f"    #[derive(Clone, PartialEq, Message)]\n"
                f"    struct {fixture}",
                spawn,
                fixture,
            )
        for test in (
            "old_clients_and_new_servers_interoperate_on_all_spawn_messages",
            "new_clients_dual_write_fallback_for_old_servers_on_all_spawn_messages",
        ):
            self.assertIn(
                f'#[cfg(feature = "client")]\n    #[test]\n    fn {test}()',
                spawn,
                test,
            )


if __name__ == "__main__":
    unittest.main()
