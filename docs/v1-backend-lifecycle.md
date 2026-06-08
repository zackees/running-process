# v1 Backend Lifecycle

The broker coordinates backend startup, recovery, idle shutdown, and process
tree cleanup. Backends still own service-specific work and data-plane RPC.

## Lifecycle States

| State | Meaning |
|---|---|
| `absent` | No live backend exists for the service/version. |
| `spawning` | One broker task holds the spawn lock. |
| `running` | Backend is ready and has a direct pipe. |
| `draining` | Backend is exiting after idle or shutdown policy. |
| `failed` | Backend failed and spawn budget controls retry. |

## Spawn Coordination

The broker serializes startup by service/version:

1. Acquire a per-service/version file lock.
2. Verify the lock file identity after acquisition.
3. Re-check the backend table under the lock.
4. Launch the backend under platform lifetime control.
5. Wait for the backend pipe to become ready.
6. Record the manifest and lifecycle event.
7. Release the lock.

The wait loop is adaptive and bounded. It keys progress to the spawning process
instead of sleeping for a fixed interval.

## Spawn Budget

Each service/version has a bounded spawn budget. Repeated crashes or failed
starts produce `ERROR_RATE_LIMITED` with `retry_after_ms`. A successful stable
backend replenishes the budget according to policy.

## Recovery

The broker treats these as stale backend entries:

- process id no longer exists
- process start identity does not match the manifest
- boot id differs from the current boot
- backend pipe is gone
- executable hash differs from manifest policy

Stale entries are removed before a new spawn attempt.

## Idle Shutdown

Backends report or expose activity through their direct data plane. The broker
uses the backend's last-active timestamp and service policy to trigger idle
shutdown. The idle clock is monotonic and platform-specific:

| Platform | Clock |
|---|---|
| Linux | `CLOCK_BOOTTIME` |
| macOS | `CLOCK_UPTIME_RAW` |
| Windows | `GetTickCount64` |

## Parent-Death Cleanup

| Platform | Mechanism |
|---|---|
| Windows | Job object with kill-on-close and `CREATE_BREAKAWAY_FROM_JOB`. |
| Linux | Child registers `PR_SET_PDEATHSIG` immediately after fork. |
| macOS | Supervisor child watches the parent with `kqueue(NOTE_EXIT)`. |

The broker starts lifetime control before it exposes a backend pipe.

## Graceful Broker Shutdown

1. Stop accepting new `Hello` requests.
2. Return `ERROR_SHUTTING_DOWN` for in-flight startup work that has not
   committed a backend.
3. Leave already-routed client-to-backend connections alone.
4. Drop spawn locks.
5. Cancel partial child startup.
6. Drain for 10 seconds.
7. Force exit.

## Lifecycle Events

Every transition emits a bounded `LifecycleEvent`. See
[v1 lifecycle events](v1-lifecycle-events.md).
