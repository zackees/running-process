# Independent spawning: implementation evidence

Tracking: running-process#1202 and zackees/kernal-api#189. Neither issue is
complete. This record preserves the pre-implementation RED evidence; it is
not a claim of scheduler, broker, or facade support.

## Baseline, 2026-09-13

Backend base: `0e39d9d` (4.10.11). Facade base: `1e7e6bc` (0.1.0), which
still pins backend 4.10.10. Work branches are `feat/independent-spawn` and
`feat/public-spawn-mode`, respectively. The backend checkout is a worktree at
`../kernal-api-extern/running-process` beside the facade checkout.

Added `crates/running-process/tests/spawn_placement.rs`. Build with:

```sh
soldr cargo test -p running-process --no-default-features --test spawn_placement --no-run
```

Run the emitted test executable under a temporary, memory-limited user unit
(substitute the executable path printed by Cargo and a fresh unit name):

```sh
systemd-run --user --wait --pipe --collect \
  --unit=rp-1202-red-1789294723 \
  --property=MemoryMax=96M --property=MemoryAccounting=yes \
  /path/to/spawn_placement-test \
  --exact legacy_detach_does_not_satisfy_independent_placement --ignored --nocapture
```

Observed exit 101, with caller and detached child both in:

```text
0::/user.slice/user-1000.slice/user@1000.service/app.slice/rp-1202-red-1789294723.service
memory.max = 100663296
memory.current = 1818624
```

The `assert_ne!` fails because both processes have identical cgroup membership.
The fixture kills and reaps its child before asserting. `memory.current` is a
post-cleanup reading of the worker's accounting, not a measurement of the
child's allocation. A separate normal regression test confirms that legacy
detached spawning retains inherited membership. The intentionally failing
historical repro is ignored in ordinary test runs.

The host has an accessible, running systemd user manager. The transient unit
is collected after completion. There is no Windows or Docker execution
evidence yet.

## Implementation constraints and next work

- Preserve legacy spawning; add explicit Inherited/Independent semantics.
  Independent must never silently use the legacy Windows breakaway fallback.
- Implement placement verification and native scheduler launch/control in the
  substrate. Verify cgroup subtree separation on Linux and actual Job Object
  separation on Windows; PPID changes are insufficient.
- Keep launch payloads out of scheduler names, logs, and public registration
  metadata. Transport explicit argv, environment, cwd and supported stdio;
  reject unsupported inheritance and lifetime combinations before launch.
- Investigate the existing `SpawnDaemon` request in
  `crates/running-process-protocol/proto/daemon.proto` for the external-broker
  path. It currently carries a single command string and reports a PID and
  floating-point creation time. It does not yet provide the required exact
  argv, placement, reuse-safe control, cancellation or readiness contract.
  The client currently has lazy daemon startup; Independent must explicitly
  connect to an already-available, correctly placed broker.
- Define bounded scheduling/readiness/cancellation, cleanup ownership, process
  identity and handle/drop/stop semantics before exposing success. Test
  partial success and concurrent requests without duplicate launches.
- Extend the real Linux fixture to measure child memory, worker teardown and
  surviving independent lifetime; add restrictive Windows Job Object and
  non-systemd Docker fixtures, including outer-container memory enforcement.
- Add a focused facade RED test before facade implementation. Keep all public
  types facade-owned and all launching behind the private backend adapter.
- Run broader checks and review; open mutually linked implementation PRs.
  Publish the backend first, then update the facade's exact released pin,
  merge its PR and publish its release. Do not release local path patches.

Completion requires both issues resolved by PRs and the release cascade
verified. These remaining items must not be replaced by the baseline repro.
