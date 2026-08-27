# Kernel substrate contract

Issue #1147 provides the adoption gate for `kernal-api` (spelling retained for
the consuming repository). `running-process` remains the lower native layer:
it must never depend on `kernal-api`.

The only supported consumer spelling is:

```toml
running-process = { version = "4.10.6", default-features = false, features = ["kernel-substrate"] }
```

`kernel-substrate` is exactly `async-process`. It is semantic on purpose:
consumers must not couple to private feature names or implementation crates.
The #1146 structural guard owns feature decomposition; this contract adds the
resolver-derived adoption ratchet and does not replace it.

## Dependency decision record

`ci.kernel_substrate_contract` invokes the sanctioned
`soldr cargo tree -p running-process --no-default-features --features
kernel-substrate --edges normal,build --prefix none` command. It accepts only
the package names and rationales checked into
`docs/kernel-substrate-allowlist.toml`; comments, dependency aliases,
dev-dependencies, target-specific declarations, and build edges cannot widen
the selected resolved graph unnoticed. The forbidden set explicitly excludes
protobuf/protocol generation, SQLite, PTY, downloader/archive/hash tooling,
CLI/config parsing, and a `kernal-api` edge. Adding any package needs a reviewed
allowlist rationale.

## Timing evidence

`ci.kernel_substrate_timing` runs an isolated clean `soldr cargo check` then
the exact same command against that temporary target directory. Its JSON record
contains schema version, command, feature, package, lock hash, toolchain, exit
codes, resolved package names, and nanosecond durations; its checked-in schema
is `docs/kernel-substrate-timing.schema.json`. CI uploads Linux evidence but deliberately has
no absolute wall-clock threshold: runner load is not a dependency contract.
The record is compared by reviewers against future baselines rather than used
as a brittle pass/fail timer.
