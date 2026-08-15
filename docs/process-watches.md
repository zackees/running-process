# Process watches

Process watches diagnose commands that recursively launch tools or produce a
short-lived failing descendant. Watches are configured before `start()` so
Linux can establish tracing without an attach-after-spawn race.

```python
from running_process import (
    ObservationPolicy,
    ProcessWatch,
    RunningProcess,
    StackCapture,
    StackDump,
)

process = RunningProcess(
    ["soldr", "build"],
    process_observation=ObservationPolicy.REQUIRE_EXACT,
    process_watches=[
        ProcessWatch.on_exec(
            basename="soldr",
            dump=StackDump(capture=StackCapture.ORIGIN_REQUIRED),
            label="recursive-soldr",
        ),
        ProcessWatch.on_exit(
            code=-1,
            dump=StackDump(capture=StackCapture.ORIGIN_PREFERRED),
            label="minus-one-exit",
        ),
    ],
)

for match in process.watch_matches:
    print(match.watch.label, match.event, match.dump)
```

`on_exec(basename="soldr")` is an exact basename comparison. Use `path=` for
an exact path; using both is an error. `on_exit(code=-1)` deliberately matches
Unix status 255 and the Windows signed representation while retaining the raw
platform status in the event. `limit=1` is the safe default; set `limit=None`
explicitly for an unbounded watch.

## Reliability and stack provenance

Event reliability and stack provenance are separate. Call
`RunningProcess.process_observation_capabilities()` before launch when useful,
then inspect `process.process_observation` for the backend actually selected.

| Platform | Non-invasive backend | Exact policy |
|---|---|---|
| Linux | `/proc` reconciliation (`SnapshotInferred`) | launch-time `ptrace` (`ExactTrace`) |
| macOS | `kqueue` wake-up plus snapshot reconciliation (`KernelHintReconciled`) | unavailable without a future authorized Endpoint Security provider |
| Windows | Job Object/IOCP (`KernelNotification`) | unavailable until the dedicated `DEBUG_PROCESS` supervisor is enabled |

Linux origin capture is a bounded register and stack-memory artifact from the
actual spawning tracee thread at fork/clone time. The tracee is resumed before
the artifact is written or delivered to Python. Deferred `.rpstack` artifacts
are intentionally raw and report `REMOTE_SPAWNING_THREAD`; unavailable owner or
origin captures return an honest `DumpResult.error` and `CaptureSource.NONE`
instead of substituting a misleading stack.

`ALLOW_TRACING` uses exact tracing when available and records a fallback reason
otherwise. `REQUIRE_EXACT` fails before `start()` on unsupported platforms.
Linux launch can still fail if ptrace is denied by an LSM, seccomp policy, a
container policy, or another debugger.

## Live consumption

Open the cursor before starting for lossless-from-launch consumption. Cursors
return immutable matches, explicit `ProcessWatchGap` values if their bounded
retention window is overrun, and `None` at terminal EOF.

```python
process = RunningProcess(command, auto_run=False, process_watches=[watch])
cursor = process.process_watch_cursor()
process.start()

for item in cursor:
    print(item)
```

`AsyncRunningProcess.process_watch_cursor()` provides the same values through
`async for`. There is intentionally no Python callback in the native event
pump: slow Python, GIL contention, or callback exceptions cannot keep a traced
process stopped.
