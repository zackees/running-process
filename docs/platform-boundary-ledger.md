# Platform-boundary bootstrap ledger

Phase 2 of #965 freezes the current distributed host-mechanics debt before a
capability moves. The authoritative rows live in
`lints/running-process-platform-boundary/src/baseline.txt`; every row is:

```text
repo_relative_path<TAB>kind<TAB>normalized_construct<TAB>ordinal
```

The 2,314-row bootstrap union is produced from the pre-expansion Windows
Dylint dump plus the independent all-source scan. Linux and macOS Dylint dump
jobs must be merged into the same four-field sort before a ledger update lands.
The checker refuses duplicate or non-contiguous ordinals, stale paths, and
allowed-zone rows. The manifest ledger similarly ratchets target-specific and
native dependency declarations until their owning capability migrates.

## Frozen totals

| Kind | Rows |
| --- | ---: |
| `attr_cfg` | 1,378 |
| `cfg_macro` | 50 |
| `native_import` | 886 |
| **Total** | **2,314** |

| Crate | Rows |
| --- | ---: |
| `running-process` | 1,586 |
| `running-process-probe` | 346 |
| `running-process-py` | 145 |
| `running-process-probe-daemon` | 73 |
| `running-process-probe-interposer-linux` | 47 |
| `running-process-probe-interposer-macos` | 34 |
| `running-process-probe-worker` | 30 |
| `test-watchdog` | 23 |
| `running-process-probe-interposer-windows` | 16 |
| `running-process-win-gnu-bridge` | 14 |

## Capability ownership during migration

The rows retain their exact source identity; capability is assigned by the
ordered migration issue, not guessed from a directory wildcard:

- Process/containment: spawn seams, descendants, process lifecycle, and
  process-facing daemon handlers (#969).
- Terminal: PTY, ConPTY, console/input, graphics, and terminal test seams
  (#970).
- IPC: endpoint, listener, handoff, and peer-security code (#971).
- Filesystem and executable mechanics: runtime files, image discovery,
  replacement, permissions, and shadowing (#972).
- Host and user facts: environment, runtime directories, privilege, resource,
  and autostart facts (#973).
- The interposer cdylibs, Windows GNU bridge, injection, snapshot, and crash
  paths remain named specialized artifacts pending #974; their rows are not a
  general-purpose exemption for ordinary product code.

Each migration deletes only the exact rows it removes and updates both ledger
totals in the same PR. A new row is a failure, including a second identical
construct in a file that already contains one grandfathered occurrence.

## Zero baseline: what it means (#975)

The ledger is not deleted at zero. "Zero baseline" means **zero unclassified
rows**: every row that survives consolidation says why it stays, in
`ci/platform_boundary.classes.tsv` (one line per path / kind / construct; all
ordinals of that construct share the class). `ci.platform_boundary` fails on
an unclassified row, on a classification that names no ledger row, and on a
row that violates its class:

| Class | Meaning | Enforced |
| --- | --- | --- |
| `host-test` | a test or fixture asserting real host behaviour | every occurrence must be in test code |
| `compat-4x` | published 4.x surface | the note must name its 5.0 retirement |
| `artifact-format` | host selection by artifact format (PDB vs DWARF reader, unwinder per arch) | only in the probe artifact crates |

A new host `cfg` or native import is still a failure (the row would be new);
the fix is to put the mechanism behind `platform::*`, not to add a class.
`MAX_LEDGER_ROWS` in `tests/test_platform_boundary.py` is a ceiling that only
moves down. `--print-totals` reports rows / classified / unclassified.

Not done yet against #975's literal acceptance list: the ledger and the Dylint
dump escape hatch (`RUNNING_PROCESS_PLATFORM_BOUNDARY_DUMP_DIR`) still exist,
and the seeded-violation fixtures per forbidden family and the #965 final
report are open.
