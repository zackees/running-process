# Structured independent daemon requests

`independent_spawn::DaemonSpawnRequest` is the command contract for scheduler
and external-broker launches. It owns a private command and exposes literal
arguments, working directory, environment overrides/removals, and an explicit
environment clear operation. All three standard streams are null. It cannot
carry pipes, caller-owned handles or sockets, or native launch hooks.

```rust,no_run
use running_process::independent_spawn::{
    spawn_daemon_request, DaemonSpawnRequest, IndependentSpawnOptions, SpawnMode,
};

let mut request = DaemonSpawnRequest::new("my-daemon");
request.arg("--serve").env_clear().env("DAEMON_MODE", "local");
let options = IndependentSpawnOptions {
    mode: SpawnMode::Independent,
    ..Default::default()
};
let child = spawn_daemon_request(&mut request, &options)?;
# Ok::<(), running_process::independent_spawn::IndependentSpawnError>(())
```

The existing `spawn_daemon_with_mode` and `spawn_daemon_with_options` functions
retain their inherited behavior for `std::process::Command`. They explicitly
reject independent mode: Rust's command getters cannot reveal configured stdio,
environment-clear state, or native hooks, so copying the visible fields could
silently discard launch requirements. There is intentionally no conversion
from `Command` to `DaemonSpawnRequest`.

Builder inputs are checked before forwarding them to `Command`. A malformed
native string marks the request invalid, and spawning it fails without manager
submission; later setters or `env_clear()` do not erase that error. Diagnostics
do not include the rejected program, argument, or environment value.

For independent requests, `inherit_environment = false` or `env_clear()` starts
from an empty environment; explicit variables added afterward still apply.
Inherited requests retain the existing daemon environment policy unless the
request explicitly calls `env_clear()`.

Independent placement still requires a supported external launcher and verified
resource-group separation. It does not escape enclosing container, ancestor
cgroup, host memory, or security limits. This request API does not establish
application-level readiness beyond the backend's process-launch handshake.

Linux placement verification lives in the native platform substrate. It rejects
hybrid hosts whose memory controller remains on a legacy v1 hierarchy, even if
a separate v2 membership line is visible. A v2 path comparison cannot establish
memory separation for a controller attached elsewhere. This follows the
[kernel's controller association rules](https://docs.kernel.org/admin-guide/cgroup-v2.html#mounting).
Duplicate unified records, namespace-relative paths, and unavailable pidfd
support also fail explicitly rather than falling back to inherited placement.

The kernal-api module alias exposes these exact upstream items as
`kernal_api::independent_spawn::{DaemonSpawnRequest, spawn_daemon_request}`;
no matching local structs or conversion tables are needed.

Implementation and live-platform validation remain in progress for #1202.

## Authored Linux acceptance fixtures

These tests are **not yet validated**. Cargo discovers
`tests/independent_spawn_cgroup_linux.rs`; the normal test driver builds the
registered `testbins` package before running Rust tests. A direct test invocation
must likewise build the fixtures first, with the same target/profile directory.
The tests do not invoke Cargo or build missing fixtures themselves.

- `RUNNING_PROCESS_LIVE_TESTS=1` enables the inherited-cgroup baseline.
- Adding `RUNNING_PROCESS_INDEPENDENT_LIVE_TESTS=1` enables separated membership
  and caller-exit tests. A separated broker or working user scheduler is needed.
- Also setting `RUNNING_PROCESS_INDEPENDENT_TEARDOWN_TESTS=1` enables the
  cgroup-wide caller teardown fixture. Supply a writable delegated cgroup v2
  parent through `RUNNING_PROCESS_TEST_CGROUP_PARENT`. The fixture creates a
  fresh child group, moves only its gated launcher into it, then kills that
  newly owned group and requires it to become empty before requesting new
  daemon work. It never kills the supplied parent or an existing user group.
  The paired inherited-mode teardown control requires only the live and
  teardown flags, not the independent-live flag or an available broker. It
  verifies the daemon belongs to the owned group before requiring that group
  to become empty after the kill.
- Adding `RUNNING_PROCESS_INDEPENDENT_MEMORY_TESTS=1` to the live flag enables
  the 64 MiB allocation test. Use quiet, dedicated caller and target cgroups:
  unrelated memory activity can invalidate the before/after measurement.
- In a separate process, `RUNNING_PROCESS_INDEPENDENT_ABSENT_BROKER_TESTS=1`
  plus the live flag exercises a nonexistent endpoint supplied through
  `RUNNING_PROCESS_INDEPENDENT_BROKER`. Native process-control support must
  still work: an earlier seccomp/platform denial does not count as evidence
  of missing-broker handling. Do not combine this configuration with positive
  broker tests.
- For an outer-limit measurement, also supply
  `RUNNING_PROCESS_INDEPENDENT_OUTER_CGROUP` as the visible outer cgroup directory.
  It must be a strict ancestor of both groups, have a finite `memory.max`, and
  have at least 128 MiB of spare capacity. The test only reads its counters and
  limit; it neither changes limits nor deliberately triggers an OOM.

The memory fixture acknowledges startup, waits for an allocation signal, touches
64 MiB, acknowledges residency, and retains it until release. The launcher
fixture has a separate pre-launch gate so the teardown harness can place the caller
in a newly created test-owned cgroup before daemon creation. Each gate has a
deadline; successful tests require an allocation-release acknowledgement.

Current coverage distinguishes caller **process exit** from cgroup-wide teardown.
The latter uses a freshly owned group with directory-anchored control handles;
its implementation and safety checks still need live-platform validation.
Docker orchestration, validation of broker-absent failure coverage, Windows acceptance, and
actual execution remain outstanding. A supplied outer cgroup is not by itself
evidence that a test ran inside Docker or exercised hard-limit enforcement.

### Docker harness topology requirements

The helper's Linux broker entry point is
`running-process-independent-helper --broker <absolute-socket-path>`.
The socket parent must already be an owner-private directory belonging to the
broker UID; the broker refuses to unlink an existing endpoint. Its clients must
have the same UID and be visible through the process/cgroup namespace used for
verification. Request artifact paths must resolve to the same files for both
client and broker.

For the pending no-systemd acceptance harness, start the broker from the outer
supervisor before moving the test worker into its child cgroup. Keep broker and
worker below the same finite outer memory limit, but outside each other's
cgroup subtrees. Starting the broker from the worker itself would inherit the
worker's group and cannot establish the required separation. Two arbitrary
Docker containers with only a shared socket do not satisfy the shared process
visibility, artifact-path, and enclosing-limit requirements by themselves.

The existing `ci/linux_docker.py` build/lint/pytest runners are not this
acceptance harness: their presence is not evidence of the topology above.

### In-container acceptance runner (not yet executed)

`ci/independent_cgroup_acceptance.py` implements the sibling broker/worker
topology inside an already provisioned Linux environment. Once implementation
is complete and validation is allowed, invoke it with prebuilt matching binaries:

```sh
python3 ci/independent_cgroup_acceptance.py \
  --cgroup-parent /sys/fs/cgroup/DELEGATED_PARENT \
  --helper /artifacts/debug/running-process-independent-helper \
  --test-binary /artifacts/debug/deps/independent_spawn_cgroup_linux-HASH
```

Replace the parent and artifact paths with the provisioned locations. The
matching `testbin-independent-launcher` and `testbin-independent-memory-holder`
executables must also exist in `/artifacts/debug`. The runner verifies the
required test names before launching anything into a cgroup. The supplied
parent must already delegate the memory controller; the runner does not enable
controllers or change limits on that parent.

The runner creates a uniquely owned subtree capped at 512 MiB and places broker
and worker in separate child groups. Cleanup kills only that owned subtree,
waits for it to empty, and removes its empty directories. If cleanup cannot be
confirmed, broker artifacts are retained and the run fails. Neither this runner
nor the acceptance tests have been run yet.

### Docker orchestration (implemented, not yet executed)

`ci/independent_cgroup_docker_acceptance.py` invokes the runner in an existing,
operator-provisioned container through an explicit local Docker engine socket.
It does not create containers, provision delegation, or stop the operator's
container. Currently it supports Linux cgroup v2 with Docker's cgroupfs driver
and a host cgroup namespace; systemd-driver and remote-engine topologies are
not supported by this acceptance wrapper. This restriction is not a restriction
on the independent-spawn library's entire supported platform set.

The container needs a finite memory limit, a writable empty delegated child
strictly beneath its own cgroup with the memory controller enabled, Python 3,
and read-only mounts of the source and complete prebuilt artifact profile.
The operator must arrange controller delegation and an init-process leaf where
required; this wrapper never changes the container's control groups to obtain
that authority. Paths under `/sys/fs/cgroup` must match host-visible membership.

```sh
uv run --no-project python ci/independent_cgroup_docker_acceptance.py \
  --container CONTAINER_ID \
  --runner /source/ci/independent_cgroup_acceptance.py \
  --cgroup-parent /sys/fs/cgroup/docker/FULL_CONTAINER_ID/delegated \
  --helper /artifacts/debug/running-process-independent-helper \
  --test-binary /artifacts/debug/deps/independent_spawn_cgroup_linux-HASH
```

The host derives the exact container boundary from Docker's full ID and live
init PID, checks its finite limit, and rechecks the launch epoch before exec.
The in-container runner verifies its own membership and delegated-parent
containment before creating test groups. A host observation timeout does not
prove remote exec cleanup: it is reported as unconfirmed, with the container
and diagnostic artifacts retained. No Docker acceptance claim is made until
the actual run and cleanup evidence are reviewed.

### Windows caller-lifetime acceptance (authored, not executed)

`tests/independent_spawn_windows.rs` requires both
`RUNNING_PROCESS_LIVE_TESTS=1` and
`RUNNING_PROCESS_INDEPENDENT_WINDOWS_TESTS=1`, a usable Task Scheduler under
the test user, and the matching prebuilt launcher/memory-holder fixtures.
It launches the caller in inherited mode, requests an independent daemon,
confirms the caller's successful exit, and only then requests fresh 64 MiB
work from the daemon. The release acknowledgement must identify the same
fixture PID as startup. Launch failure is a test failure, not a skipped
capability result or inherited fallback.

The test holds the daemon's native process object before caller exit, records
its creation FILETIME, checks continued liveness, and waits for termination
after release. Cleanup cooperatively releases the daemon, then uses the held
native object if necessary; it never reopens a marker PID to terminate it.
The scheduler helper must also be built beside the launcher fixture.

The second case assigns the freshly launched, gated requester to a new unnamed
Job before allowing the scheduler request. Once the daemon is ready, it calls
`TerminateJobObject` on that owned Job, requires requester exit 137, and only
then asks the held daemon to perform fresh work. The harness never attaches to
or terminates an ambient Job. These are authored tests, not passing evidence.

Scheduler task cleanup and Windows memory accounting still require additional
acceptance coverage. Acquiring a handle from the fixture PID is not yet an independent
comparison against the scheduler's creation-time handshake.
