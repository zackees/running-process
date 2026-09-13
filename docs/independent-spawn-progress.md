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

The Linux native tree now also contains `scheduler_launch`: bounded
`systemd-run --user` service creation (`Type=exec`), bounded manager output,
typed unavailable-manager/permission/timeout/cancellation failures, a rollback
guard, explicit stop, and detached lifetime commit. Application data is not
put into the command: the interface accepts only a helper executable and
private IPC endpoint path. A strict pidfd path was added to the existing
`ProcessLiveness` implementation, without changing its compatibility open
behavior. Helper placement capture checks that pinned identity before and
after procfs reads.

Validation on 2026-09-13:

- Ten scheduler tests passed, including all three opt-in real-manager tests:
  placement outside the worker, uncommitted-drop rollback, and committed
  service survival until explicit stop.
- Two strict pidfd tests passed: no bare-PID fallback and ESRCH after the
  pinned target exits.
- The real placement/stop test also passed inside transient unit
  `rp-1202-green-1789295463.service` with `MemoryMax=96M` and memory accounting.
- The broader platform suite passed 90 tests, with the three real-manager
  tests ignored by default (and explicitly executed above). The spawn-path
  guard passed. The worker unit was collected and no `rp-independent-*`
  services remained afterward.

The opt-in `independent-spawn` feature now builds `running-process-launcher`
and exposes the Linux scheduler operation. The helper uses the existing
owner-private IPC layer and same-user peer credentials. Launch payloads are
bounded to 1 MiB encoded, retain native OS strings, and are never persisted.
The parent checks the helper's pinned PID, sends the explicit payload, verifies
the actual target's pinned PID and placement, and commits. The helper retains
child ownership until commit and supervises the committed target until exit.
Its uncommitted handshake lease is 30 seconds. Parent cancellation tears down
the connection and service; a late helper with no live endpoint receives no
target payload. No public SpawnMode selection or facade adapter exists yet.

Four public-substrate integration tests pass, explicitly including the three
real-systemd tests: native non-UTF-8 argv and shell-metacharacter fidelity,
explicit environment/cwd/file output and actual target stop; typed failed exec;
cancellation during an unresponsive helper handshake; and pre-cancellation.
The platform suite with the new feature passed 109 tests (three separate
real-manager primitive tests ignored by default and previously run explicitly).
Spawn-path and platform-boundary checks passed. The normal kernel-substrate
dependency tree contains no serde, serde_json, tempfile or IPC dependency from
this feature. No `rp-independent-*` services or `/tmp/rpil-*` handshake
directories remained after the tests.

Readiness now explicitly selects `ProcessStarted` or an application file
marker with exact expected bytes. Markers must be absent before launch; the
caller owns their paths and removal. A bounded readiness wait runs before
commit, followed by repeated pinned-identity/placement verification. Target
file I/O now opens regular files only, rejects symlinks/reparse points and
uses nonblocking Unix opens so a FIFO cannot stall the helper. Newly created
Unix log files use mode 0600. Eight integration tests passed, including stale
marker rejection, successful application readiness, a started target removed
on readiness timeout, and symlink-log rejection without modifying its target.

Remaining before any release: late-registration failure injection, simultaneous launch coverage,
Windows native Task Scheduler/Job Object runtime validation, external broker,
SpawnMode contracts, facade, reviews, merged PRs and the release cascade.

The public `independent_spawn_accounting` integration now runs a requester in
a real 128 MiB user service. A 24 MiB touched allocation through inherited
spawn stayed in that service and raised memory.current from 819200 to 26734592
bytes. The independent allocation was in a sibling service charged 26468352
bytes; worker usage changed from 1523712 to 1339392 bytes. The parent stopped
the entire requester unit and checked the daemon's pinned pidfd remained
alive, then stopped its independent unit and checked the same pidfd reported
exit. No requester or independent services remained. Reproduce with:

```sh
soldr cargo test -p running-process --no-default-features --features independent-spawn --test independent_spawn_accounting -- independent_allocation_survives_requester_scope_teardown --ignored --nocapture
```

Local review added stop-on-drop ownership until cleanup publication and made
the outer guard stop the worker before reading its final published unit name.
This test establishes worker accounting and teardown, not Docker outer-limit
enforcement or the unfinished canonical SpawnMode dispatch API.
Windows now selects a native Task Scheduler implementation. It registers an
on-demand, same-user InteractiveToken task with LeastPrivilege, transports
only the helper path and private endpoint in scheduler metadata, and uses the
shared bounded IPC payload/readiness handshake. The existing Windows liveness
handle supports strict pinned termination and bounded caller Job Object
membership checks. Registration is removed before returning a detached handle;
that handle owns pinned helper and target processes for explicit stop.

The Windows helper binary and focused integration-test executable both
cross-compiled successfully for x86_64-pc-windows-msvc; the focused executable
also cross-compiled for aarch64-pc-windows-msvc. This is compilation
evidence only, not evidence that Task Scheduler works or preserves the running
instance after registration removal. A dedicated Windows CI lane exercises
the actual scheduler and target lifecycle; it has not run yet. Restrictive Job
Object teardown, memory accounting, and late-registration failure injection
still need execution evidence. The eight Linux integration tests passed again
after this implementation was added.

The local `clud-review` pass (one read-only reviewer) found two high-priority
cleanup gaps before pushing. A Linux regression confirmed that a deliberately
unresponsive helper ignoring SIGTERM survived the two-second rollback client
budget (RED, exit 101). The scheduler now explicitly selects control-group
killing, one-second manager stop escalation, and SendSIGKILL. The first probe
using only a SIGTERM-ignoring target already passed because helper ownership
killed that target; the failing helper probe exercises the missing manager
policy instead. After the fix, all nine Linux integration tests passed.

The Windows review finding exposed a caller-death window between registration
and definition removal, which bypasses Rust Drop. The implementation now
registers a disabled TimeTrigger with a 45-second end boundary and a one-second
DeleteExpiredTaskAfter policy. This trigger cannot initiate a launch: its only
purpose is scheduler-owned expiry if the requester dies. Normal success still
deletes the definition immediately. XML is now constructed in memory, with a
128-bit OS-random task name, so no definition file or directory can be
abandoned. Microsoft
documents [task expiration cleanup](https://learn.microsoft.com/en-us/windows/win32/taskschd/tasksettings-deleteexpiredtaskafter)
as depending on trigger EndBoundary, and
[task deletion](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/schtasks-delete)
as not interrupting the running program. The same reviewer accepted this
design for a draft PR; runtime verification remains required before merge.
The dedicated Windows stage now tests abandoned registration without Run or
Drop as well as a full launch/definition-removal/target-stop cycle. Each test
invocation has a two-minute hard timeout after a separate build step. Do not
call this finding resolved by cross-compilation or by normal-path Drop tests.

Windows runtime evidence is now available from
[run 34754214770](https://github.com/zackees/running-process/actions/runs/34754214770):
the expiry-only policy test and abandoned-registration test passed, with actual
service deletion observed in 48.08 seconds. Earlier registration failed with
HRESULT 0x8004131a because the XML byte-encoding declaration was inappropriate
for the Unicode COM string; passing the document element without that
declaration made registration succeed. The same run's full launch test then
failed with WriteZero. Windows nonblocking byte-pipe backpressure can return
zero bytes without failure, so the private channel now normalizes that result
to WouldBlock and retries under the existing deadline. Full launch/lifetime
validation is still pending; registration expiry alone does not prove it.

Windows implementation research: Microsoft documents that
[JOBOBJECT_BASIC_PROCESS_ID_LIST](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_basic_process_id_list)
includes processes in nested child jobs. A bounded
`QueryInformationJobObject(NULL, JobObjectBasicProcessIdList, ...)` query can
test whether a pinned helper/target remains in the caller's immediate job
subtree. Do not infer separation from Task Scheduler engine parentage or
reject a target merely because it belongs to a different scheduler-owned job.
Task registration must be on-demand, same-user and non-elevated; the existing
boot-autostart helper uses ONLOGON/HIGHEST and is not the appropriate API.

The Linux native tree now contains the private `resource_placement` verifier.
Its focused tests first produced five assertion failures with an inert
implementation, then seven passes after implementation, including live self
capture. It compares cgroup path components and refuses root worker boundaries,
different cgroup/mount namespaces, malformed paths, and non-unified/hybrid
membership. It is now connected to the launcher and checks a pinned process
identity around placement capture. It does not itself prove application
readiness or survival after requester-scope teardown.

The first broader platform run was 80 passed / 1 failed: the existing
`current_executable_exposes_a_gnu_build_id` test found no GNU build ID.
`readelf -n` confirmed that the generated executable contained only the ABI
note. Revalidation with explicit `-C link-arg=-Wl,--build-id=sha1` passed all
81 tests. The spawn-path guard also passed. Keep this linker setting for the
broader suite on this host; do not delete or skip the build-ID assertion.

Scheduler implementation reference: the upstream
[systemd-run manual](https://github.com/systemd/systemd/blob/main/man/systemd-run.xml)
distinguishes transient service launch from caller-owned scope launch and
documents that the default `Type=simple` success precedes exec. Use an exec
startup handshake and target readiness/identity verification, not registration
success alone. Preserve literal argument values and keep secret payloads out
of scheduler command/description metadata.

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
