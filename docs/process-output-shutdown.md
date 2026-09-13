# Process-output shutdown

Coordinated with [kernal-api #13](https://github.com/zackees/kernal-api/issues/13)
and [its draft integration PR](https://github.com/zackees/kernal-api/pull/153).
This branch is an implementation prerequisite, not a release or completed
session-shutdown contract.

## Platform primitive

`PlatformOutput::shutdown` consumes one actor-owned output reader. On Linux and
macOS, dropping the readiness-based pipe releases its reader storage. On Windows,
a canceled Tokio read may still own a blocking task and buffer. The primitive
repeatedly requests handle-specific cancellation, polls that pending read to
completion, and drops the reader before returning. A cancellation request, a
direct-child exit, and dropping a read future are not cleanup acknowledgements.

The live reader exclusively owns the handle throughout this sequence. Retrying
also handles a blocking job that has not entered its syscall yet. No worker
thread identifier is targeted, and no new runtime or pipe implementation is
introduced. The owning actor must retain the shutdown future until completion;
an observer timeout must retain the outstanding storage allowance.

Microsoft explicitly documents that
[CancelIoEx can cancel synchronous I/O](https://devblogs.microsoft.com/oldnewthing/20170928-00/?p=97105).
Its [completion contract](https://learn.microsoft.com/en-us/windows/win32/api/ioapiset/nf-ioapiset-cancelioex)
requires observing completion separately. The current Tokio 1.52.3 lockfile's
`io/blocking.rs` recovers its buffer when a Busy read completes. Rust 1.95 maps
native error 995 to `TimedOut`, so the implementation recognizes the exact native
code rather than treating every timeout as expected cancellation.

## Local evidence

- Focused RED: missing shutdown method, Soldr log `20260913T120326Z`.
- Focused Linux GREEN: two tests/helper cases. The regression establishes a
  pending read on a silent live child, then requires shutdown before killing it.
- Strict Linux platform Clippy passes with `--deny warnings`.
- Windows x86-64 MSVC library and test cross-builds pass. This is not native execution.
- The sync API snapshot gate has an existing mismatch: its recorded Rust exports
  omit `spawn_contract` and `spawn_dispatch` exports already present at the branch
  base. This change touches neither the snapshot inputs nor those exports; the
  unrelated snapshot is left unchanged. This is not a passing snapshot gate.
- Linux platform suite: 101 passed, three existing systemd-dependent tests
  ignored. The default local linker omitted the GNU build ID required by an
  existing test. Explicitly relinking the test harness with the following
  command supplies it; `readelf -n` confirms the note. No test was weakened.

```sh
SOLDR_LINKER=default soldr --no-cache cargo rustc --locked \
  -p running-process-platform-internal --no-default-features \
  --features async-process --lib --profile test -j1 \
  -- -C link-arg=-Wl,--build-id=sha1
```

Run the resulting native test harness directly after this command.

## Required integration

The session actor must retain and join both pump tasks, interrupt blocked sends
and reads, invoke this primitive on shutdown and post-exit abandonment, and
discard queued events before publishing output cleanup completion. Store the
request and result independently of an observer future so cancellation/retry
cannot lose cleanup ownership. Signal shutdown before acquiring an output
consumer lock. Keep direct-child kill/reap separate.

Required regressions include a full queue with no consumer, a descendant-held
silent pipe, queued-before-syscall cancellation, canceled/retried observers, and
concurrent shutdown callers. Native Windows execution and the full six-target
acceptance remain outstanding. Until that integration lands, existing session
Drop, wait, and output exhaustion do not acquire a stronger reclamation promise.
