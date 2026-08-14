# Platform-boundary Dylint

This pre-expansion lint enforces the host-platform boundary defined in #965.
It sees inactive target branches and every module delivered to the compiler;
the private implementation roots in `running-process-platform-internal` are
the only allowed native zones.

`src/baseline.txt` is an exact-occurrence ratchet. Its 2,314 bootstrap rows
combine the Windows Dylint dump with the independent all-source scan: 1,378
host attributes, 50 host `cfg!` expressions, and 886 native imports. Rows are
keyed by path, kind, normalized construct, and ordinal; a second matching
occurrence in a grandfathered file is therefore rejected.

To regenerate a host dump while deliberately changing the baseline, set
`RUNNING_PROCESS_PLATFORM_BOUNDARY_DUMP_DIR` to an empty directory and run the
platform lint with its pinned nightly. Each compiler process writes a separate
dump; merge and sort those files by all four ledger fields. Linux, macOS, and
Windows dumps must be reconciled before changing the committed ledger.

The CI workspace gate explicitly loads this package before the repository-wide
Dylint discovery pass, so an earlier unrelated lint failure cannot silently
skip the platform boundary.
